//! The storage language swap planned with embedding's own text rule.

use std::collections::HashSet;

use rusqlite::Connection;

use crate::embedding::text::TextRule;
use crate::embedding::EMBEDDING_DIM;
use crate::model::{Diff, NodeRecord};
use crate::storage::connection::open_staging;
use crate::storage::language_swap::{plan, swap, SwapBookkeeping};
use crate::storage::write::apply_diff;

/// A rust node `id` in `file`, with `signature`.
fn node_in(id: &str, file: &str, signature: &str) -> NodeRecord {
    let mut node = NodeRecord::new(id, "Function", id, id, file, "rust");
    node.signature = Some(signature.to_string());
    node
}

fn column(conn: &Connection, sql: &str) -> Vec<String> {
    conn.prepare(sql)
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn bookkeeping(capable: &HashSet<String>) -> SwapBookkeeping<'_> {
    SwapBookkeeping {
        language: "rust",
        plugin_fingerprint: "fp",
        semantic_pass_languages: capable,
        retried: false,
    }
}

/// A language swap planned with the real [`TextRule`]: a generated staged
/// node is never staged for embedding (new, or turned generated), its
/// sibling is, the live vector of the node that turned generated is
/// deleted, an unchanged node keeps its vector, and live ends with the flag.
/// Controls: `EmbedInput::from_row` passing `generated: false` (`y` is
/// staged and `x` keeps its vector); drop `generated` from
/// `storage::language_swap::NODE_COLUMNS` (live's `x` stays unflagged and
/// keeps its vector).
#[test]
fn a_swap_stages_no_generated_node_and_deletes_the_vector_of_one_that_turned_generated() {
    let generated = |id: &str| {
        let mut node = node_in(id, "pb.rs", &format!("fn {id}()"));
        node.generated = true;
        node
    };
    let dir = tempfile::tempdir().unwrap();
    let live_path = dir.path().join("index.db");
    let staging_path = dir.path().join("staging-rust.db");
    let mut live = open_staging(&live_path).unwrap();
    live.execute(
        "INSERT INTO meta (id, schema_version, indexer_version, lastUsed) VALUES (1, 'x', 'x', 'x')",
        [],
    )
    .unwrap();
    apply_diff(
        &mut live,
        &Diff {
            upsert_nodes: vec![node_in("x", "pb.rs", "fn x()"), node_in("keep", "a.rs", "fn keep()")],
            ..Default::default()
        },
    )
    .unwrap();
    for id in ["x", "keep"] {
        crate::storage::vectors::insert(&live, id, &[0.0; EMBEDDING_DIM], "model").unwrap();
    }
    let mut staging = open_staging(&staging_path).unwrap();
    apply_diff(
        &mut staging,
        &Diff {
            upsert_nodes: vec![
                generated("x"),
                node_in("keep", "a.rs", "fn keep()"),
                generated("y"),
                node_in("z", "a.rs", "fn z()"),
            ],
            ..Default::default()
        },
    )
    .unwrap();

    let planned =
        plan(&mut staging, live_path.to_str().unwrap(), "rust", &TextRule { version: "model" }, true)
            .unwrap();
    drop(staging);
    let mut staged: Vec<&str> = planned.to_embed.upsert_nodes.iter().map(|node| node.id.as_str()).collect();
    staged.sort_unstable();
    assert_eq!(staged, vec!["z"], "only the new hand-written node is owed a vector");

    swap(&mut live, &staging_path, None, &bookkeeping(&HashSet::new())).unwrap();
    assert_eq!(column(&live, "SELECT nodeId FROM vectors ORDER BY nodeId"), vec!["keep"]);
    assert_eq!(
        column(&live, "SELECT id FROM nodes WHERE generated = 1 ORDER BY id"),
        vec!["x", "y"],
        "the swap carries the flag into live"
    );
}
