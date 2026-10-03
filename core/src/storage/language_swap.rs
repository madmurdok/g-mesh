//! The plan and the swap of a per-language reindex walked into a staging
//! index: [`plan`] compares the staging file with the live one and records
//! only the difference in staging's own plan tables, without touching live;
//! [`swap`] applies that difference to live in one transaction.
//! Design: [ADR 0008](../../../docs/adr/0008-workspace-reindex-staging-swap.md).
//!
//! Every table keyed by one language's rows appears in both halves: `nodes`,
//! `declarations`, `placeholder_targets`, `qualified_suffixes`,
//! `untyped_calls`, `edges`, `containers` and `vectors`. A table missing from either keeps stale rows
//! after a swap.
//!
//! The unchanged-node rule decides whose edges a node gets. A node is
//! unchanged when it is in both indexes with every `nodes` column equal and
//! its `declarations`, `placeholder_targets`, `qualified_suffixes` and
//! `untyped_calls` rows equal, which is exactly "in both and not in `plan_upsert_nodes`". An unchanged node keeps all of
//! its live outgoing edges, whatever their `source`, and gets none of
//! staging's, so the swap neither downgrades what a semantic pass wrote nor
//! brings back a structural edge the pass retracted. Two exceptions: a live
//! syntactic edge whose staged edge of the same id differs is replaced by
//! it; and a live edge whose target does not exist after the swap is
//! deleted, and the staged edges from that node of the same kind, which
//! live lacks or holds only with a vanished target, are taken in its place.
//! Every other node, changed or new, gets exactly staging's outgoing edges;
//! the semantic pass that runs after the swap refines them.
//!
//! A live pending-symbol placeholder of the language that staging lacks is
//! kept, not deleted, when the caller says the language's semantic tier is
//! swept (`keep_semantic_placeholders`): a semantic pass adds such nodes under
//! ids no walk emits, so their absence from staging says nothing about them.
//! It stays until something re-sends it or a complete whole-project semantic
//! pass does not (`IndexStore::sweep_unclaimed_nodes`). A kept node survives
//! the swap for its edges exactly as an unchanged node does. Placeholders
//! only: a declaration the walk no longer emits is deleted at the swap.
//!
//! Neither half enables foreign keys or depends on them: the swap deletes
//! edges before nodes and inserts nodes before edges, so it is also valid on
//! a connection that enforces them.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::embedding::pipeline::ComputedEmbedding;
use crate::embedding::text::text_to_embed;
use crate::embedding::EmbeddingPipeline;
use crate::graph::symbol_links::PENDING_SYMBOL_NATIVE_KIND;
use crate::storage::schema;
use crate::storage::write::{Diff, NodeRecord};

const NODE_COLUMNS: &str = "id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, \
     signature, visibility, visibilityContainer, docComment, language, nativeKind, hasSyntaxErrors, container, \
     qualifiedPath";
const DECLARATION_COLUMNS: &str = "nodeId, ordinal, startLine, startCol, endLine, endCol, signature, hasBody";
const TARGET_COLUMNS: &str = "nodeId, scopeKind, scope, keyKind, key, fromContainer, fromFile, keyPath";
const SUFFIX_COLUMNS: &str = "suffix, nodeId";
const UNTYPED_COLUMNS: &str = "name, nodeId";
const EDGE_COLUMNS: &str = "id, fromId, toId, kind, source, engine, resolved, toDeclaration";
const CONTAINER_COLUMNS: &str = "nodeId, language, key, parentKey, memberCount";

/// The plan tables, created in the staging file. `plan_text_changed` holds
/// the upserted nodes whose embedded text differs from live's, whose live
/// vector is therefore stale; `plan_unchanged_nodes` the nodes the
/// unchanged-node rule (module doc) keeps live's outgoing edges for;
/// `plan_pending_files` the files whose edges the swap leaves structural until
/// the language's semantic pass refreshes them (ADR 0009); `plan_keep_nodes`
/// the live placeholders staging lacks that the swap keeps (module doc).
const PLAN_DDL: &str = "
DROP TABLE IF EXISTS plan_delete_nodes;
DROP TABLE IF EXISTS plan_upsert_nodes;
DROP TABLE IF EXISTS plan_unchanged_nodes;
DROP TABLE IF EXISTS plan_text_changed;
DROP TABLE IF EXISTS plan_delete_edges;
DROP TABLE IF EXISTS plan_upsert_edges;
DROP TABLE IF EXISTS plan_delete_containers;
DROP TABLE IF EXISTS plan_upsert_containers;
DROP TABLE IF EXISTS plan_pending_files;
DROP TABLE IF EXISTS plan_keep_nodes;
CREATE TABLE plan_delete_nodes (id TEXT PRIMARY KEY);
CREATE TABLE plan_upsert_nodes (id TEXT PRIMARY KEY);
CREATE TABLE plan_unchanged_nodes (id TEXT PRIMARY KEY);
CREATE TABLE plan_text_changed (id TEXT PRIMARY KEY);
CREATE TABLE plan_delete_edges (id TEXT PRIMARY KEY);
CREATE TABLE plan_upsert_edges (id TEXT PRIMARY KEY);
CREATE TABLE plan_delete_containers (nodeId TEXT PRIMARY KEY);
CREATE TABLE plan_upsert_containers (nodeId TEXT PRIMARY KEY);
CREATE TABLE plan_pending_files (filePath TEXT PRIMARY KEY);
CREATE TABLE plan_keep_nodes (id TEXT PRIMARY KEY);
";

/// Row counts of one plan, for the reindex's log line and for tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PlanCounts {
    pub delete_nodes: usize,
    pub upsert_nodes: usize,
    pub delete_edges: usize,
    pub upsert_edges: usize,
    pub delete_containers: usize,
    pub upsert_containers: usize,
    /// Files whose edges stay structural until the semantic pass.
    pub pending_files: usize,
    /// Live placeholders staging lacks, kept for the semantic pass to settle.
    pub keep_nodes: usize,
}

impl PlanCounts {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// What [`plan`] leaves for the caller: the counts, and the upserted nodes
/// that need a vector computed (id, doc comment and signature only) as a
/// [`Diff`] for [`EmbeddingPipeline::compute`].
pub struct Plan {
    pub counts: PlanCounts,
    pub to_embed: Diff,
}

/// `path` (absolute) as a read-only SQLite URI: `file:///C:/...` on Windows,
/// `file:///home/...` elsewhere, with the characters a URI gives a meaning
/// escaped.
fn read_only_uri(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len() + 1);
    if !path.starts_with('/') {
        escaped.push('/');
    }
    for c in path.chars() {
        match c {
            '\\' => escaped.push('/'),
            '%' => escaped.push_str("%25"),
            '?' => escaped.push_str("%3f"),
            '#' => escaped.push_str("%23"),
            _ => escaped.push(c),
        }
    }
    format!("file://{escaped}?mode=ro")
}

/// Compares `staging` (one freshly walked and linked language) with the live
/// index at `live_path`, attached read-only, and writes the plan tables into
/// `staging`. Reads live inside one transaction, so the plan describes one
/// snapshot of it. `embedding_version` is the pipeline's, for spotting a live
/// vector made by another model. `keep_semantic_placeholders`: whether the
/// live placeholders staging lacks are kept (module doc).
pub fn plan(
    staging: &mut Connection,
    live_path: &str,
    language: &str,
    embedding_version: &str,
    keep_semantic_placeholders: bool,
) -> Result<Plan> {
    staging
        .execute("ATTACH DATABASE ?1 AS live", params![read_only_uri(live_path)])
        .with_context(|| format!("failed to attach the live index {live_path} to the staging index"))?;
    let planned = plan_attached(staging, language, embedding_version, keep_semantic_placeholders);
    let detached = staging.execute("DETACH DATABASE live", []);
    let planned = planned?;
    detached.context("failed to detach the live index from the staging index")?;
    Ok(planned)
}

fn plan_attached(
    staging: &mut Connection,
    language: &str,
    embedding_version: &str,
    keep_semantic_placeholders: bool,
) -> Result<Plan> {
    let tx = staging.transaction().context("failed to start the plan transaction")?;
    tx.execute_batch(PLAN_DDL).context("failed to create the plan tables")?;

    // Staging holds only the language being reindexed, so only the live side
    // of a statement filters by it.
    let run = |sql: &str, what: &str| -> Result<usize> {
        let changed =
            if sql.contains("?1") { tx.execute(sql, params![language]) } else { tx.execute(sql, []) };
        changed.with_context(|| format!("failed to plan {what}"))
    };

    // The language's nodes after the swap are the staged ones and the kept
    // ones, so "an endpoint survives" means "is a staged or a kept node".
    let keep_nodes = if keep_semantic_placeholders {
        run(
            &format!(
                "INSERT INTO plan_keep_nodes (id)
                 SELECT id FROM live.nodes
                 WHERE language = ?1 AND kind = 'Module' AND nativeKind = '{PENDING_SYMBOL_NATIVE_KIND}'
                   AND id NOT IN (SELECT id FROM main.nodes)"
            ),
            "the placeholders to keep",
        )?
    } else {
        0
    };
    let delete_nodes = run(
        "INSERT INTO plan_delete_nodes (id)
         SELECT id FROM live.nodes WHERE language = ?1 AND id NOT IN (SELECT id FROM main.nodes)
           AND id NOT IN (SELECT id FROM plan_keep_nodes)",
        "the nodes to delete",
    )?;
    run(
        &format!(
            "INSERT OR IGNORE INTO plan_upsert_nodes (id) SELECT id FROM (
                 SELECT {NODE_COLUMNS} FROM main.nodes WHERE language = ?1
                 EXCEPT SELECT {NODE_COLUMNS} FROM live.nodes WHERE language = ?1)"
        ),
        "the nodes to upsert",
    )?;
    // A node whose declarations or placeholder target differ is upserted too:
    // the swap replaces both child tables wholesale for every upserted node.
    for (table, columns) in [
        ("declarations", DECLARATION_COLUMNS),
        ("placeholder_targets", TARGET_COLUMNS),
        ("qualified_suffixes", SUFFIX_COLUMNS),
        ("untyped_calls", UNTYPED_COLUMNS),
    ] {
        run(
            &format!(
                "INSERT OR IGNORE INTO plan_upsert_nodes (id) SELECT nodeId FROM (
                     SELECT {columns} FROM main.{table}
                     EXCEPT SELECT {columns} FROM live.{table} WHERE nodeId IN (SELECT id FROM main.nodes))"
            ),
            &format!("the staged {table} that changed"),
        )?;
        run(
            &format!(
                "INSERT OR IGNORE INTO plan_upsert_nodes (id) SELECT nodeId FROM (
                     SELECT {columns} FROM live.{table} WHERE nodeId IN (SELECT id FROM main.nodes)
                     EXCEPT SELECT {columns} FROM main.{table})"
            ),
            &format!("the live {table} that went away"),
        )?;
    }
    let upsert_nodes: usize =
        tx.query_row("SELECT COUNT(*) FROM plan_upsert_nodes", [], |row| row.get::<_, i64>(0))? as usize;

    run(
        "INSERT INTO plan_unchanged_nodes (id)
         SELECT s.id FROM main.nodes s
         WHERE s.id IN (SELECT id FROM live.nodes WHERE language = ?1)
           AND s.id NOT IN (SELECT id FROM plan_upsert_nodes)",
        "the unchanged nodes",
    )?;

    // Whether node `x` exists after the swap: it is staged, kept, or a live
    // node of another language, which the swap does not touch.
    let survives = |x: &str| {
        format!(
            "({x} IN (SELECT id FROM main.nodes)
              OR {x} IN (SELECT id FROM plan_keep_nodes)
              OR EXISTS (SELECT 1 FROM live.nodes o WHERE o.id = {x} AND o.language <> ?1))"
        )
    };

    // Live edges of the language: either endpoint is one of its live nodes.
    // One from an unchanged or a kept node goes only when its target does;
    // any other goes when staging lacks its id.
    let delete_edges = run(
        &format!(
            "INSERT INTO plan_delete_edges (id)
             SELECT e.id FROM live.edges e
             WHERE (e.fromId IN (SELECT id FROM live.nodes WHERE language = ?1)
                    OR e.toId IN (SELECT id FROM live.nodes WHERE language = ?1))
               AND CASE WHEN e.fromId IN (SELECT id FROM plan_unchanged_nodes)
                             OR e.fromId IN (SELECT id FROM plan_keep_nodes)
                        THEN NOT {target_survives}
                        ELSE e.id NOT IN (SELECT id FROM main.edges) END",
            target_survives = survives("e.toId"),
        ),
        "the edges to delete",
    )?;
    // Staged edges that differ from live. One from an unchanged node is taken
    // over a live syntactic edge of its own id, or in place of a live edge of
    // the same kind whose target went away, and never over any other live
    // edge of its own id that is kept.
    let upsert_edges = run(
        &format!(
            "INSERT INTO plan_upsert_edges (id) SELECT s.id FROM (
                 SELECT {EDGE_COLUMNS} FROM main.edges
                 EXCEPT SELECT {EDGE_COLUMNS} FROM live.edges WHERE id IN (SELECT id FROM main.edges)) s
             WHERE s.fromId NOT IN (SELECT id FROM plan_unchanged_nodes)
                OR EXISTS (SELECT 1 FROM live.edges t WHERE t.id = s.id AND t.source = 'syntactic')
                OR (EXISTS (SELECT 1 FROM live.edges d
                            WHERE d.fromId = s.fromId AND d.kind = s.kind AND NOT {dropped_survives})
                    AND NOT EXISTS (SELECT 1 FROM live.edges l
                                    WHERE l.id = s.id AND {kept_survives}))",
            dropped_survives = survives("d.toId"),
            kept_survives = survives("l.toId"),
        ),
        "the edges to upsert",
    )?;

    // A staged node whose own row changed, or whose outgoing edges the swap
    // changes, has edges only the semantic pass can make final. Only files
    // the walk still has count: a path with no `File` node names nothing a
    // response could show.
    let pending_files = run(
        "INSERT OR IGNORE INTO plan_pending_files (filePath)
         SELECT n.filePath FROM main.nodes n
         WHERE (n.id IN (SELECT id FROM plan_upsert_nodes)
                OR n.id IN (SELECT e.fromId FROM main.edges e
                            WHERE e.id IN (SELECT id FROM plan_upsert_edges))
                OR n.id IN (SELECT e.fromId FROM live.edges e
                            WHERE e.id IN (SELECT id FROM plan_delete_edges)))
           AND n.filePath IN (SELECT filePath FROM main.nodes WHERE kind = 'File')",
        "the files left pending",
    )?;

    let delete_containers = run(
        "INSERT INTO plan_delete_containers (nodeId)
         SELECT nodeId FROM live.containers
         WHERE language = ?1 AND nodeId NOT IN (SELECT nodeId FROM main.containers)",
        "the containers to delete",
    )?;
    let upsert_containers = run(
        &format!(
            "INSERT INTO plan_upsert_containers (nodeId) SELECT nodeId FROM (
                 SELECT {CONTAINER_COLUMNS} FROM main.containers WHERE language = ?1
                 EXCEPT SELECT {CONTAINER_COLUMNS} FROM live.containers WHERE language = ?1)"
        ),
        "the containers to upsert",
    )?;

    let to_embed = plan_embeddings(&tx, embedding_version)?;
    tx.commit().context("failed to commit the plan")?;

    Ok(Plan {
        counts: PlanCounts {
            delete_nodes,
            upsert_nodes,
            delete_edges,
            upsert_edges,
            delete_containers,
            upsert_containers,
            pending_files,
            keep_nodes,
        },
        to_embed,
    })
}

/// Fills `plan_text_changed` and returns the upserted nodes owed a vector:
/// new nodes, nodes whose embedded text changed, and nodes whose live vector
/// is missing or was made by another model. Every other node keeps its live
/// vector, which is the vector its unchanged text would get again.
fn plan_embeddings(tx: &Connection, embedding_version: &str) -> Result<Diff> {
    let mut statement = tx
        .prepare(
            "SELECT s.id, s.docComment, s.signature, l.id IS NOT NULL, l.docComment, l.signature,
                    v.embeddingVersion
             FROM plan_upsert_nodes p
             JOIN main.nodes s ON s.id = p.id
             LEFT JOIN live.nodes l ON l.id = s.id
             LEFT JOIN live.vectors v ON v.nodeId = s.id",
        )
        .context("failed to prepare the embedding plan")?;
    type Row = (String, Option<String>, Option<String>, bool, Option<String>, Option<String>, Option<String>);
    let rows: Vec<Row> = statement
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?))
        })
        .context("failed to read the embedding plan")?
        .collect::<rusqlite::Result<_>>()
        .context("failed to read the embedding plan")?;
    drop(statement);

    let mut to_embed = Diff::default();
    for (id, doc, signature, in_live, live_doc, live_signature, live_version) in rows {
        let staged_text = text_to_embed(doc.as_deref(), signature.as_deref());
        let text_changed =
            in_live && text_to_embed(live_doc.as_deref(), live_signature.as_deref()) != staged_text;
        if text_changed {
            tx.execute("INSERT INTO plan_text_changed (id) VALUES (?1)", params![id])
                .context("failed to record a node whose text changed")?;
        }
        let owed = !in_live || text_changed || live_version.as_deref() != Some(embedding_version);
        if staged_text.is_some() && owed {
            let mut node = NodeRecord::new(id.clone(), "", id.clone(), id, "", "");
            node.doc_comment = doc;
            node.signature = signature;
            to_embed.upsert_nodes.push(node);
        }
    }
    Ok(to_embed)
}

/// Everything [`swap`] writes besides the planned rows.
pub struct SwapBookkeeping<'a> {
    pub language: &'a str,
    /// `language_state.pluginFingerprint` of the walk.
    pub plugin_fingerprint: &'a str,
    /// The languages whose manifests declare a semantic pass, for the
    /// semantic-pass roll-up.
    pub semantic_pass_languages: &'a std::collections::HashSet<String>,
}

/// Applies the plan in the staging file at `staging_path` to `live`, in one
/// transaction: the planned deletes, then the planned upserts copied from
/// staging, the vectors of deleted nodes and of nodes whose text changed
/// removed and `vectors` stored, `language_state` of the language written
/// (walked now, semantic pass owed), both meta roll-ups reconciled, the
/// language's `pending_reindex` row removed and, for a language with a
/// semantic pass, its semantic-pending rows written. A failure rolls all of it
/// back. Returns the ids of the placeholders the plan kept.
pub fn swap(
    live: &mut Connection,
    staging_path: &Path,
    vectors: Option<(&EmbeddingPipeline, &[ComputedEmbedding])>,
    bookkeeping: &SwapBookkeeping<'_>,
) -> Result<Vec<String>> {
    let staging_path = staging_path.to_str().context("the staging index path is not valid UTF-8")?;
    live.execute("ATTACH DATABASE ?1 AS staging", params![staging_path])
        .with_context(|| format!("failed to attach the staging index {staging_path}"))?;
    let swapped = swap_attached(live, vectors, bookkeeping);
    let detached = live.execute("DETACH DATABASE staging", []);
    let kept = swapped?;
    detached.context("failed to detach the staging index")?;
    Ok(kept)
}

fn swap_attached(
    live: &mut Connection,
    vectors: Option<(&EmbeddingPipeline, &[ComputedEmbedding])>,
    bookkeeping: &SwapBookkeeping<'_>,
) -> Result<Vec<String>> {
    let tx = live.transaction().context("failed to start the swap transaction")?;
    let run = |sql: &str, what: &str| -> Result<usize> {
        tx.execute(sql, []).with_context(|| format!("failed to swap in {what}"))
    };

    // Deletes first, edges before nodes and child rows before their node.
    run("DELETE FROM edges WHERE id IN (SELECT id FROM staging.plan_delete_edges)", "the edge deletes")?;
    run(
        "DELETE FROM vectors WHERE nodeId IN (SELECT id FROM staging.plan_delete_nodes)
            OR nodeId IN (SELECT id FROM staging.plan_text_changed)",
        "the stale vectors",
    )?;
    for table in ["declarations", "placeholder_targets", "qualified_suffixes", "untyped_calls"] {
        run(
            &format!(
                "DELETE FROM {table} WHERE nodeId IN (SELECT id FROM staging.plan_delete_nodes)
                    OR nodeId IN (SELECT id FROM staging.plan_upsert_nodes)"
            ),
            table,
        )?;
    }
    run(
        "DELETE FROM containers WHERE nodeId IN (SELECT nodeId FROM staging.plan_delete_containers)
            OR nodeId IN (SELECT id FROM staging.plan_delete_nodes)",
        "the container deletes",
    )?;
    run("DELETE FROM nodes WHERE id IN (SELECT id FROM staging.plan_delete_nodes)", "the node deletes")?;

    let node_updates = NODE_COLUMNS
        .split(", ")
        .filter(|column| *column != "id")
        .map(|column| format!("{column} = excluded.{column}"))
        .collect::<Vec<_>>()
        .join(", ");
    run(
        &format!(
            "INSERT INTO nodes ({NODE_COLUMNS})
             SELECT {NODE_COLUMNS} FROM staging.nodes WHERE id IN (SELECT id FROM staging.plan_upsert_nodes)
             ON CONFLICT(id) DO UPDATE SET {node_updates}"
        ),
        "the node upserts",
    )?;
    run(
        &format!(
            "INSERT INTO declarations ({DECLARATION_COLUMNS})
             SELECT {DECLARATION_COLUMNS} FROM staging.declarations
             WHERE nodeId IN (SELECT id FROM staging.plan_upsert_nodes)"
        ),
        "the declarations",
    )?;
    run(
        &format!(
            "INSERT INTO placeholder_targets ({TARGET_COLUMNS})
             SELECT {TARGET_COLUMNS} FROM staging.placeholder_targets
             WHERE nodeId IN (SELECT id FROM staging.plan_upsert_nodes)"
        ),
        "the placeholder targets",
    )?;
    run(
        &format!(
            "INSERT INTO qualified_suffixes ({SUFFIX_COLUMNS})
             SELECT {SUFFIX_COLUMNS} FROM staging.qualified_suffixes
             WHERE nodeId IN (SELECT id FROM staging.plan_upsert_nodes)"
        ),
        "the qualified suffixes",
    )?;
    run(
        &format!(
            "INSERT INTO untyped_calls ({UNTYPED_COLUMNS})
             SELECT {UNTYPED_COLUMNS} FROM staging.untyped_calls
             WHERE nodeId IN (SELECT id FROM staging.plan_upsert_nodes)"
        ),
        "the untyped calls",
    )?;
    run(
        &format!(
            "INSERT INTO edges ({EDGE_COLUMNS})
             SELECT {EDGE_COLUMNS} FROM staging.edges WHERE id IN (SELECT id FROM staging.plan_upsert_edges)
             ON CONFLICT(id) DO UPDATE SET fromId = excluded.fromId, toId = excluded.toId,
                kind = excluded.kind, source = excluded.source, engine = excluded.engine,
                resolved = excluded.resolved, toDeclaration = excluded.toDeclaration"
        ),
        "the edge upserts",
    )?;
    run(
        &format!(
            "INSERT INTO containers ({CONTAINER_COLUMNS})
             SELECT {CONTAINER_COLUMNS} FROM staging.containers
             WHERE nodeId IN (SELECT nodeId FROM staging.plan_upsert_containers)
             ON CONFLICT(nodeId) DO UPDATE SET language = excluded.language, key = excluded.key,
                parentKey = excluded.parentKey, memberCount = excluded.memberCount"
        ),
        "the container upserts",
    )?;

    if let Some((embedding, computed)) = vectors {
        embedding.store(&tx, computed);
    }

    let SwapBookkeeping { language, plugin_fingerprint, semantic_pass_languages } = bookkeeping;
    tx.execute(
        "INSERT INTO language_state (language, bulkIndexedAt, pluginFingerprint, semanticPassAt)
         VALUES (?1, CURRENT_TIMESTAMP, ?2, NULL)
         ON CONFLICT(language) DO UPDATE SET bulkIndexedAt = excluded.bulkIndexedAt,
            pluginFingerprint = excluded.pluginFingerprint, semanticPassAt = NULL",
        params![language, plugin_fingerprint],
    )
    .with_context(|| format!("failed to record that {language} was reindexed"))?;
    // Inside the swap's transaction: a committed swap always has its rows, a
    // rolled-back one never does. The language row is written even with no
    // files, since unchanged call sites still owe the pass.
    if semantic_pass_languages.contains(*language) {
        tx.execute(
            "INSERT OR REPLACE INTO semantic_pending (language, since)
             VALUES (?1, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))",
            params![language],
        )
        .with_context(|| format!("failed to record {language}'s pending semantic pass"))?;
        tx.execute(
            "INSERT OR IGNORE INTO semantic_pending_files (language, filePath)
             SELECT ?1, filePath FROM staging.plan_pending_files",
            params![language],
        )
        .with_context(|| format!("failed to record {language}'s pending files"))?;
    }
    schema::record_bulk_index(&tx).context("failed to reconcile the bulk-index roll-up")?;
    schema::reconcile_semantic_pass_rollup(&tx, semantic_pass_languages)
        .context("failed to reconcile the semantic-pass roll-up")?;
    tx.execute("DELETE FROM pending_reindex WHERE language = ?1", params![language])
        .with_context(|| format!("failed to clear {language}'s pending reindex"))?;

    let kept: Vec<String> = tx
        .prepare("SELECT id FROM staging.plan_keep_nodes ORDER BY id")
        .and_then(|mut statement| statement.query_map([], |row| row.get(0))?.collect())
        .context("failed to read the kept placeholders")?;
    tx.commit().context("failed to commit the swap")?;
    Ok(kept)
}

/// Deletes those of `ids` that are still pending-symbol placeholders of
/// `language`, with their edges (either end), vectors, declarations,
/// placeholder targets, qualified suffixes and container rows, in one
/// transaction, and returns how many nodes went.
pub(crate) fn delete_placeholders(conn: &mut Connection, language: &str, ids: &[String]) -> Result<usize> {
    let tx = conn.transaction().context("failed to start the placeholder sweep")?;
    let mut deleted = 0;
    for id in ids {
        let placeholder: bool = tx
            .query_row(
                &format!(
                    "SELECT EXISTS (SELECT 1 FROM nodes WHERE id = ?1 AND language = ?2 AND kind = 'Module'
                                    AND nativeKind = '{PENDING_SYMBOL_NATIVE_KIND}')"
                ),
                params![id, language],
                |row| row.get(0),
            )
            .context("failed to look up a kept placeholder")?;
        if !placeholder {
            continue;
        }
        for sql in [
            "DELETE FROM edges WHERE fromId = ?1 OR toId = ?1",
            "DELETE FROM vectors WHERE nodeId = ?1",
            "DELETE FROM declarations WHERE nodeId = ?1",
            "DELETE FROM placeholder_targets WHERE nodeId = ?1",
            "DELETE FROM qualified_suffixes WHERE nodeId = ?1",
            "DELETE FROM untyped_calls WHERE nodeId = ?1",
            "DELETE FROM containers WHERE nodeId = ?1",
        ] {
            tx.execute(sql, params![id]).context("failed to delete a kept placeholder's rows")?;
        }
        deleted += tx
            .execute("DELETE FROM nodes WHERE id = ?1", params![id])
            .context("failed to delete a kept placeholder")?;
    }
    tx.commit().context("failed to commit the placeholder sweep")?;
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::storage::connection::open_staging;
    use crate::storage::write::{apply_diff, EdgeRecord, NodeRecord};

    #[test]
    fn a_path_becomes_a_read_only_uri_on_either_platform() {
        assert_eq!(read_only_uri("/tmp/a b/index.db"), "file:///tmp/a b/index.db?mode=ro");
        assert_eq!(read_only_uri(r"C:\Users\x\index.db"), "file:///C:/Users/x/index.db?mode=ro");
        assert_eq!(read_only_uri("/p/50%?#.db"), "file:///p/50%25%3f%23.db?mode=ro");
    }

    fn node(id: &str) -> NodeRecord {
        NodeRecord::new(id, "Function", id, id, "src/a.rs", "rust")
    }

    /// Through files whose directory holds characters the URI escapes (not
    /// `?`, which Windows refuses in a file name):
    /// the plan reads live without writing it, and the swap brings live to
    /// staging's rows of the language and leaves another language alone.
    #[test]
    fn a_plan_and_swap_through_an_awkward_path_replaces_only_the_language() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().join("a #b %d");
        std::fs::create_dir_all(&dir).unwrap();
        let live_path = dir.join("index.db");
        let staging_path = dir.join("staging-rust.db");

        let mut live = open_staging(&live_path).unwrap();
        live.execute(
            "INSERT INTO meta (id, schema_version, indexer_version, lastUsed) VALUES (1, 'x', 'x', 'x')",
            [],
        )
        .unwrap();
        let mut other = NodeRecord::new("go-1", "Function", "g", "g", "a.go", "go");
        other.doc_comment = Some("go".to_string());
        apply_diff(
            &mut live,
            &Diff { upsert_nodes: vec![node("old"), node("same"), other], ..Default::default() },
        )
        .unwrap();
        let mut staging = open_staging(&staging_path).unwrap();
        apply_diff(
            &mut staging,
            &Diff { upsert_nodes: vec![node("same"), node("new")], ..Default::default() },
        )
        .unwrap();

        let plan = plan(&mut staging, live_path.to_str().unwrap(), "rust", "model", true).unwrap();
        drop(staging);
        assert_eq!(
            plan.counts,
            PlanCounts { delete_nodes: 1, upsert_nodes: 1, ..Default::default() },
            "one node gone, one new, one unchanged"
        );

        let capable = HashSet::new();
        swap(
            &mut live,
            &staging_path,
            None,
            &SwapBookkeeping {
                language: "rust",
                plugin_fingerprint: "fp",
                semantic_pass_languages: &capable,
            },
        )
        .unwrap();
        let ids: Vec<String> = live
            .prepare("SELECT id FROM nodes ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, vec!["go-1", "new", "same"]);
    }

    /// A rust node `id` in `file`, with `signature`.
    fn node_in(id: &str, file: &str, signature: &str) -> NodeRecord {
        let mut node = NodeRecord::new(id, "Function", id, id, file, "rust");
        node.signature = Some(signature.to_string());
        node
    }

    fn file_node(file: &str) -> NodeRecord {
        NodeRecord::new(format!("file-{file}"), "File", file, file, file, "rust")
    }

    fn column(conn: &Connection, sql: &str) -> Vec<String> {
        conn.prepare(sql)
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// Live: `a` (its signature changes), `b` (unchanged, its edge's target
    /// goes), `c` (unchanged), `d` (every node deleted). Staging adds a node
    /// at a path with no `File` node, as a placeholder has. Returns the
    /// directory, the live connection, the staging path and the plan.
    fn planned_reindex() -> (tempfile::TempDir, Connection, std::path::PathBuf, Plan) {
        let dir = tempfile::tempdir().unwrap();
        let live_path = dir.path().join("index.db");
        let staging_path = dir.path().join("staging-rust.db");

        let mut live = open_staging(&live_path).unwrap();
        live.execute(
            "INSERT INTO meta (id, schema_version, indexer_version, lastUsed) VALUES (1, 'x', 'x', 'x')",
            [],
        )
        .unwrap();
        let files = ["a.rs", "b.rs", "c.rs", "d.rs"];
        let mut upsert_nodes: Vec<NodeRecord> = files.iter().map(|f| file_node(f)).collect();
        upsert_nodes.extend([
            node_in("a1", "a.rs", "fn a()"),
            node_in("b1", "b.rs", "fn b()"),
            node_in("c1", "c.rs", "fn c()"),
            node_in("d1", "d.rs", "fn d()"),
        ]);
        apply_diff(
            &mut live,
            &Diff {
                upsert_nodes,
                upsert_edges: vec![
                    EdgeRecord::new("b1->d1", "b1", "d1", "CALLS", "semantic", true),
                    EdgeRecord::new("c1->b1", "c1", "b1", "CALLS", "semantic", true),
                ],
                ..Default::default()
            },
        )
        .unwrap();

        let mut staging = open_staging(&staging_path).unwrap();
        let mut upsert_nodes: Vec<NodeRecord> = files[..3].iter().map(|f| file_node(f)).collect();
        upsert_nodes.extend([
            node_in("a1", "a.rs", "fn a(x: u32)"),
            node_in("b1", "b.rs", "fn b()"),
            node_in("c1", "c.rs", "fn c()"),
            NodeRecord::new("placeholder", "Function", "p", "p", "external/p.rs", "rust"),
        ]);
        apply_diff(
            &mut staging,
            &Diff {
                upsert_nodes,
                upsert_edges: vec![EdgeRecord::new("c1->b1", "c1", "b1", "CALLS", "tree-sitter", true)],
                ..Default::default()
            },
        )
        .unwrap();
        let plan = plan(&mut staging, live_path.to_str().unwrap(), "rust", "model", true).unwrap();
        (dir, live, staging_path, plan)
    }

    /// The plan names exactly the changed file and the file whose edges the
    /// swap changed. Controls: drop the `plan_upsert_nodes` arm (`a.rs`
    /// missing); drop the two edge arms (`b.rs` missing); drop the `File`
    /// restriction (`external/p.rs` appears).
    #[test]
    fn the_plan_records_the_files_left_pending() {
        let (_dir, _live, staging_path, plan) = planned_reindex();

        let staging = open_staging(&staging_path).unwrap();
        assert_eq!(
            column(&staging, "SELECT filePath FROM plan_pending_files ORDER BY filePath"),
            vec!["a.rs", "b.rs"]
        );
        assert_eq!(plan.counts.pending_files, 2);
    }

    fn bookkeeping<'a>(capable: &'a HashSet<String>) -> SwapBookkeeping<'a> {
        SwapBookkeeping { language: "rust", plugin_fingerprint: "fp", semantic_pass_languages: capable }
    }

    /// A committed swap of a semantic-pass language writes its pending row
    /// and files. Control: drop the capability gate (the non-capable swap in
    /// the next test gets rows).
    #[test]
    fn a_swap_writes_the_pending_rows() {
        let (_dir, mut live, staging_path, _plan) = planned_reindex();
        let capable: HashSet<String> = HashSet::from(["rust".to_string()]);

        swap(&mut live, &staging_path, None, &bookkeeping(&capable)).unwrap();

        assert_eq!(column(&live, "SELECT language FROM semantic_pending"), vec!["rust"]);
        let since = column(&live, "SELECT since FROM semantic_pending").remove(0);
        assert!(
            since.len() == 20 && since.ends_with('Z') && since.as_bytes()[10] == b'T',
            "RFC 3339 UTC: {since}"
        );
        assert_eq!(
            column(
                &live,
                "SELECT filePath FROM semantic_pending_files WHERE language = 'rust' ORDER BY filePath"
            ),
            vec!["a.rs", "b.rs"]
        );
    }

    /// A language with no semantic pass owes nothing: no rows.
    #[test]
    fn a_swap_of_a_language_without_a_semantic_pass_writes_no_pending_rows() {
        let (_dir, mut live, staging_path, _plan) = planned_reindex();

        swap(&mut live, &staging_path, None, &bookkeeping(&HashSet::new())).unwrap();

        assert!(column(&live, "SELECT language FROM semantic_pending").is_empty());
        assert!(column(&live, "SELECT filePath FROM semantic_pending_files").is_empty());
    }

    /// A swap that fails after its pending rows are written rolls them back
    /// with everything else. Control: write the rows outside the swap's
    /// transaction (on `live` in [`swap`], before [`swap_attached`] opens
    /// it) - they survive the failure.
    #[test]
    fn a_failed_swap_leaves_no_pending_rows() {
        let (_dir, mut live, staging_path, _plan) = planned_reindex();
        live.execute(
            "INSERT INTO pending_reindex (language, trigger, startedAt) VALUES ('rust', 'x', 'x')",
            [],
        )
        .unwrap();
        live.execute_batch(
            "CREATE TRIGGER fail_swap BEFORE DELETE ON pending_reindex BEGIN SELECT RAISE(ABORT, 'forced'); END;",
        )
        .unwrap();
        let capable: HashSet<String> = HashSet::from(["rust".to_string()]);

        swap(&mut live, &staging_path, None, &bookkeeping(&capable)).expect_err("the trigger fails the swap");

        assert!(column(&live, "SELECT language FROM semantic_pending").is_empty());
        assert!(column(&live, "SELECT filePath FROM semantic_pending_files").is_empty());
    }

    fn qpath(first: &str, rest: &[(&str, &str)]) -> crate::protocol::types::QualifiedPath {
        rest.iter().fold(crate::protocol::types::QualifiedPath::root(first), |path, (sep, name)| {
            path.child(*sep, *name)
        })
    }

    /// `read` in `module`, as `<S as Read>::read`, aliased as `<alias>::read`.
    fn trait_impl_method(module: &str, alias: &str) -> NodeRecord {
        let path = qpath(module, &[("::", "<S as Read>"), ("::", "read")]);
        let mut node = NodeRecord::new("read", "Function", "read", path.display(), "src/a.rs", "rust");
        node.qualified_path = Some(path);
        node.alias_paths = vec![qpath(module, &[("::", alias), ("::", "read")])];
        node
    }

    /// Live holds `live_node`, staging `staged_node` (same id); plans and
    /// swaps, and returns live with the plan's counts.
    fn swap_one(
        live_node: NodeRecord,
        staged_node: NodeRecord,
    ) -> (tempfile::TempDir, Connection, PlanCounts) {
        let dir = tempfile::tempdir().unwrap();
        let live_path = dir.path().join("index.db");
        let staging_path = dir.path().join("staging-rust.db");
        let mut live = open_staging(&live_path).unwrap();
        live.execute(
            "INSERT INTO meta (id, schema_version, indexer_version, lastUsed) VALUES (1, 'x', 'x', 'x')",
            [],
        )
        .unwrap();
        apply_diff(&mut live, &Diff { upsert_nodes: vec![live_node], ..Default::default() }).unwrap();
        let mut staging = open_staging(&staging_path).unwrap();
        apply_diff(&mut staging, &Diff { upsert_nodes: vec![staged_node], ..Default::default() }).unwrap();
        let planned = plan(&mut staging, live_path.to_str().unwrap(), "rust", "model", true).unwrap();
        drop(staging);
        swap(&mut live, &staging_path, None, &bookkeeping(&HashSet::new())).unwrap();
        (dir, live, planned.counts)
    }

    /// A node whose only change is an alias is re-swapped, and live ends with
    /// staging's suffix rows. Control: drop `qualified_suffixes` from
    /// `plan_attached`'s child-table loop (upsert count 0, old `S::read`
    /// stays), or from `swap_attached`'s delete loop or its insert (rows
    /// duplicated or missing).
    #[test]
    fn a_swap_carries_an_alias_only_change() {
        let (_dir, live, counts) = swap_one(trait_impl_method("m", "S"), trait_impl_method("m", "T"));

        assert_eq!(counts.upsert_nodes, 1, "the alias change alone marks the node upserted");
        assert_eq!(
            column(&live, "SELECT suffix FROM qualified_suffixes ORDER BY suffix"),
            vec!["<S as Read>::read", "T::read", "m::T::read"]
        );
    }

    /// A path change reaches live's `nodes.qualifiedPath`. Control: drop
    /// `qualifiedPath` from `NODE_COLUMNS` (live keeps `m`'s path).
    #[test]
    fn a_swap_carries_a_path_change_into_the_node_row() {
        let (_dir, live, _counts) = swap_one(trait_impl_method("m", "S"), trait_impl_method("n", "S"));

        let stored = column(&live, "SELECT qualifiedPath FROM nodes WHERE id = 'read'").remove(0);
        assert!(stored.starts_with("n\u{1f}"), "{stored:?}");
    }

    /// An unchanged node with a path is not re-swapped.
    #[test]
    fn an_unchanged_node_with_a_path_is_left_alone() {
        let (_dir, live, counts) = swap_one(trait_impl_method("m", "S"), trait_impl_method("m", "S"));

        assert_eq!(counts, PlanCounts::default());
        assert_eq!(column(&live, "SELECT suffix FROM qualified_suffixes ORDER BY suffix").len(), 3);
    }

    /// A `qualifiedName`-keyed placeholder for `a::T.f`, its key path either
    /// `a`, `::T`, `.f` (`split`) or `a`, `::T.f`; both join to the same key.
    fn keyed_placeholder(split: bool) -> NodeRecord {
        let mut node = NodeRecord::new("p1", "Module", "f", "a.rs#f", "src/b.rs", "rust");
        node.native_kind = Some(PENDING_SYMBOL_NATIVE_KIND.to_string());
        node.target = Some(crate::storage::write::PlaceholderTargetRecord {
            scope_kind: "file".to_string(),
            scope: "src/a.rs".to_string(),
            key_kind: "qualifiedName".to_string(),
            key: "a::T.f".to_string(),
            from_container: None,
            key_path: Some(if split {
                qpath("a", &[("::", "T"), (".", "f")])
            } else {
                qpath("a", &[("::", "T.f")])
            }),
        });
        node
    }

    /// A placeholder whose only change is its `keyPath` is re-swapped, and
    /// live ends with staging's. Control: drop `keyPath` from
    /// `TARGET_COLUMNS` (upsert count 0, live keeps the old path).
    #[test]
    fn a_swap_carries_a_key_path_only_change() {
        let (_dir, live, counts) = swap_one(keyed_placeholder(false), keyed_placeholder(true));

        assert_eq!(counts.upsert_nodes, 1, "the keyPath change alone marks the placeholder upserted");
        assert_eq!(
            column(&live, "SELECT keyPath FROM placeholder_targets WHERE nodeId = 'p1'"),
            vec!["a\u{1f}::\u{1f}T\u{1f}.\u{1f}f"]
        );
    }

    /// A swept placeholder takes any suffix rows under its id with it.
    /// Control: drop the `qualified_suffixes` delete from
    /// `delete_placeholders` (the row remains).
    #[test]
    fn deleting_a_kept_placeholder_takes_its_suffix_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = open_staging(&dir.path().join("index.db")).unwrap();
        let mut placeholder = NodeRecord::new("p1", "Module", "f", "a.rs#f", "src/b.rs", "rust");
        placeholder.native_kind = Some(PENDING_SYMBOL_NATIVE_KIND.to_string());
        apply_diff(&mut conn, &Diff { upsert_nodes: vec![placeholder], ..Default::default() }).unwrap();
        conn.execute("INSERT INTO qualified_suffixes (suffix, nodeId) VALUES ('x::f', 'p1')", []).unwrap();

        assert_eq!(delete_placeholders(&mut conn, "rust", &["p1".to_string()]).unwrap(), 1);
        assert!(column(&conn, "SELECT suffix FROM qualified_suffixes").is_empty());
    }
}
