//! The plan and the swap of a per-language reindex walked into a staging
//! index: [`plan`] compares the staging file with the live one and records
//! only the difference in staging's own plan tables, without touching live;
//! [`swap`] applies that difference to live in one transaction.
//! Design: [ADR 0008](../../../docs/adr/0008-workspace-reindex-staging-swap.md).
//!
//! Every table keyed by one language's rows appears in both halves: `nodes`,
//! `declarations`, `placeholder_targets`, `edges`, `containers` and
//! `vectors`. A table missing from either keeps stale rows after a swap.
//!
//! The semantic-edge rule: a live edge whose `source` is `semantic` and whose
//! two endpoints both survive the swap is kept as it is, whether staging has
//! a structural edge under the same id or none at all. A semantic pass
//! upgrades structural edges in place and adds edges of its own, and neither
//! kind is in a structural walk; the pass that runs after the swap replaces
//! them.
//!
//! Neither half enables foreign keys or depends on them: the swap deletes
//! edges before nodes and inserts nodes before edges, so it is also valid on
//! a connection that enforces them.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::embedding::pipeline::{text_to_embed, ComputedEmbedding};
use crate::embedding::EmbeddingPipeline;
use crate::storage::schema;
use crate::storage::write::{Diff, NodeRecord};

const NODE_COLUMNS: &str = "id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, \
     signature, visibility, visibilityContainer, docComment, language, nativeKind, hasSyntaxErrors, container";
const DECLARATION_COLUMNS: &str = "nodeId, ordinal, startLine, startCol, endLine, endCol, signature, hasBody";
const TARGET_COLUMNS: &str = "nodeId, scopeKind, scope, keyKind, key, fromContainer, fromFile";
const EDGE_COLUMNS: &str = "id, fromId, toId, kind, source, engine, resolved, toDeclaration";
const CONTAINER_COLUMNS: &str = "nodeId, language, key, parentKey, memberCount";

/// The plan tables, created in the staging file. `plan_text_changed` holds
/// the upserted nodes whose embedded text differs from live's, whose live
/// vector is therefore stale.
const PLAN_DDL: &str = "
DROP TABLE IF EXISTS plan_delete_nodes;
DROP TABLE IF EXISTS plan_upsert_nodes;
DROP TABLE IF EXISTS plan_text_changed;
DROP TABLE IF EXISTS plan_delete_edges;
DROP TABLE IF EXISTS plan_upsert_edges;
DROP TABLE IF EXISTS plan_delete_containers;
DROP TABLE IF EXISTS plan_upsert_containers;
CREATE TABLE plan_delete_nodes (id TEXT PRIMARY KEY);
CREATE TABLE plan_upsert_nodes (id TEXT PRIMARY KEY);
CREATE TABLE plan_text_changed (id TEXT PRIMARY KEY);
CREATE TABLE plan_delete_edges (id TEXT PRIMARY KEY);
CREATE TABLE plan_upsert_edges (id TEXT PRIMARY KEY);
CREATE TABLE plan_delete_containers (nodeId TEXT PRIMARY KEY);
CREATE TABLE plan_upsert_containers (nodeId TEXT PRIMARY KEY);
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
/// vector made by another model.
pub fn plan(
    staging: &mut Connection,
    live_path: &str,
    language: &str,
    embedding_version: &str,
) -> Result<Plan> {
    staging
        .execute("ATTACH DATABASE ?1 AS live", params![read_only_uri(live_path)])
        .with_context(|| format!("failed to attach the live index {live_path} to the staging index"))?;
    let planned = plan_attached(staging, language, embedding_version);
    let detached = staging.execute("DETACH DATABASE live", []);
    let planned = planned?;
    detached.context("failed to detach the live index from the staging index")?;
    Ok(planned)
}

fn plan_attached(staging: &mut Connection, language: &str, embedding_version: &str) -> Result<Plan> {
    let tx = staging.transaction().context("failed to start the plan transaction")?;
    tx.execute_batch(PLAN_DDL).context("failed to create the plan tables")?;

    // Staging holds only the language being reindexed, so only the live side
    // of a statement filters by it.
    let run = |sql: &str, what: &str| -> Result<usize> {
        let changed =
            if sql.contains("?1") { tx.execute(sql, params![language]) } else { tx.execute(sql, []) };
        changed.with_context(|| format!("failed to plan {what}"))
    };

    // Staged nodes are exactly the language's nodes after the swap, so "an
    // endpoint survives" means "is a staged node".
    let delete_nodes = run(
        "INSERT INTO plan_delete_nodes (id)
         SELECT id FROM live.nodes WHERE language = ?1 AND id NOT IN (SELECT id FROM main.nodes)",
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
    for (table, columns) in [("declarations", DECLARATION_COLUMNS), ("placeholder_targets", TARGET_COLUMNS)] {
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

    // Live edges of the language: either endpoint is one of its live nodes.
    let delete_edges = run(
        "INSERT INTO plan_delete_edges (id)
         SELECT e.id FROM live.edges e
         WHERE (e.fromId IN (SELECT id FROM live.nodes WHERE language = ?1)
                OR e.toId IN (SELECT id FROM live.nodes WHERE language = ?1))
           AND e.id NOT IN (SELECT id FROM main.edges)
           AND NOT (e.source = 'semantic'
                    AND e.fromId IN (SELECT id FROM main.nodes)
                    AND e.toId IN (SELECT id FROM main.nodes))",
        "the edges to delete",
    )?;
    let upsert_edges = run(
        &format!(
            "INSERT INTO plan_upsert_edges (id) SELECT s.id FROM (
                 SELECT {EDGE_COLUMNS} FROM main.edges
                 EXCEPT SELECT {EDGE_COLUMNS} FROM live.edges WHERE id IN (SELECT id FROM main.edges)) s
             WHERE NOT EXISTS (
                 SELECT 1 FROM live.edges l
                 WHERE l.id = s.id AND l.source = 'semantic'
                   AND l.fromId IN (SELECT id FROM main.nodes)
                   AND l.toId IN (SELECT id FROM main.nodes))"
        ),
        "the edges to upsert",
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
/// (walked now, semantic pass owed), both meta roll-ups reconciled and the
/// language's `pending_reindex` row removed. A failure rolls all of it back.
pub fn swap(
    live: &mut Connection,
    staging_path: &Path,
    vectors: Option<(&EmbeddingPipeline, &[ComputedEmbedding])>,
    bookkeeping: &SwapBookkeeping<'_>,
) -> Result<()> {
    let staging_path = staging_path.to_str().context("the staging index path is not valid UTF-8")?;
    live.execute("ATTACH DATABASE ?1 AS staging", params![staging_path])
        .with_context(|| format!("failed to attach the staging index {staging_path}"))?;
    let swapped = swap_attached(live, vectors, bookkeeping);
    let detached = live.execute("DETACH DATABASE staging", []);
    swapped?;
    detached.context("failed to detach the staging index")?;
    Ok(())
}

fn swap_attached(
    live: &mut Connection,
    vectors: Option<(&EmbeddingPipeline, &[ComputedEmbedding])>,
    bookkeeping: &SwapBookkeeping<'_>,
) -> Result<()> {
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
    for table in ["declarations", "placeholder_targets"] {
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
    schema::record_bulk_index(&tx).context("failed to reconcile the bulk-index roll-up")?;
    schema::reconcile_semantic_pass_rollup(&tx, semantic_pass_languages)
        .context("failed to reconcile the semantic-pass roll-up")?;
    tx.execute("DELETE FROM pending_reindex WHERE language = ?1", params![language])
        .with_context(|| format!("failed to clear {language}'s pending reindex"))?;

    tx.commit().context("failed to commit the swap")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::storage::connection::open_staging;
    use crate::storage::write::{apply_diff, NodeRecord};

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

        let plan = plan(&mut staging, live_path.to_str().unwrap(), "rust", "model").unwrap();
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
}
