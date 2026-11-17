//! Container row maintenance inside `apply_diff`'s transaction: the write
//! half of logical containers. The design lives in `graph::containers`' module doc.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::model::container_id;
use crate::model::kinds::{DEFINES_KIND, MODULE_KIND, NON_DECLARATION_NATIVE_KINDS};
use crate::model::{Diff, NodeRecord};
use g_mesh_wire::native_kind::CONTAINER_NATIVE_KIND;

/// `edges.engine` on the `DEFINES` edges written here. A diagnostic label only
/// (`storage::schema`'s DDL comment on `edges`): it says "core wrote this, no
/// plugin did", which is what anyone reading a `DEFINES` edge out of a
/// container needs to know.
const CORE_ENGINE: &str = "g-mesh-core";

/// A container's identity: `(language, key)`. Keys are only unique within a
/// language - a Go import path and a Rust module path can be the same string -
/// so the pair is the unit everywhere, matching `containers`' own `UNIQUE
/// (language, key)`.
type Key = (String, String);

/// The id of the `DEFINES` edge from `container_node_id` to `member_id`,
/// the space-joined hash of `edge <from> DEFINES <to>`. This is core's own
/// scheme, not the NUL-joined one in `plugins/sdk/src/ids.rs`; it cannot
/// collide with a plugin edge id because the `from` half is a container id no
/// plugin node can have (see [`container_id`]).
fn defines_edge_id(container_node_id: &str, member_id: &str) -> String {
    let digest = Sha256::digest(format!("edge {container_node_id} {DEFINES_KIND} {member_id}").as_bytes());
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// Whether a node of this `nativeKind` with this `container` is a member, and
/// of which container. See the module doc's "Who counts as a member".
fn membership(language: &str, container: Option<&str>, native_kind: Option<&str>) -> Option<Key> {
    let key = container.filter(|key| !key.is_empty())?;
    let excluded = native_kind.is_some_and(|kind| NON_DECLARATION_NATIVE_KINDS.contains(&kind));
    (!excluded).then(|| (language.to_string(), key.to_string()))
}

fn record_membership(node: &NodeRecord) -> Option<Key> {
    membership(&node.language, node.container.as_deref(), node.native_kind.as_deref())
}

/// What [`detach`] worked out before `apply_diff` wrote anything, for
/// [`attach`] to finish once it has.
pub(crate) struct Pending {
    /// Every member the diff leaves in a container, one entry per id, in the
    /// order of that id's last upsert.
    members: Vec<(String, Key)>,
    /// `(container, containerParent)` for every member record in the diff, in
    /// diff order - *all* records, not only each id's last one, so a stream
    /// applied in one batch and the same stream cut into several agree on who
    /// wrote a container's parent last.
    parents: Vec<(Key, Option<String>)>,
    /// Every container whose member set this diff may have changed.
    touched: BTreeSet<Key>,
}

/// Phase one, run inside `apply_diff`'s transaction before any delete or
/// upsert: removes the `DEFINES` edge of every node that the diff takes out of
/// its current container. See the module doc for why this cannot wait until
/// after the node writes.
///
/// `None` when the diff cannot touch any container: none of its upserts carry
/// one, and the index holds no container either - so none of the nodes it names
/// can currently be a member. That is every diff from a language without
/// containers (all of TS/JS) in a project with no containered language, and it
/// costs one lookup of an empty table rather than one per node.
pub(crate) fn detach(conn: &Connection, diff: &Diff) -> Result<Option<Pending>> {
    let any_upsert_joins = diff.upsert_nodes.iter().any(|node| record_membership(node).is_some());
    if !any_upsert_joins {
        let any_container: bool = conn
            .prepare_cached("SELECT EXISTS (SELECT 1 FROM containers)")
            .context("failed to prepare the any-container check")?
            .query_row([], |row| row.get(0))
            .context("failed to check whether the index holds any container")?;
        if !any_container {
            return Ok(None);
        }
    }

    // What the diff leaves each upserted id with: its last record wins, the
    // same way the last `INSERT ... ON CONFLICT DO UPDATE` does.
    let mut final_membership: HashMap<&str, Option<Key>> = HashMap::new();
    let mut parents = Vec::new();
    for node in &diff.upsert_nodes {
        let joined = record_membership(node);
        if let Some(key) = &joined {
            parents.push((key.clone(), node.container_parent.clone()));
        }
        final_membership.insert(node.id.as_str(), joined);
    }

    let mut touched = BTreeSet::new();
    {
        let mut current = conn
            .prepare_cached("SELECT language, container, nativeKind FROM nodes WHERE id = ?1")
            .context("failed to prepare the current-membership lookup")?;
        let mut drop_edge = conn
            .prepare_cached("DELETE FROM edges WHERE id = ?1")
            .context("failed to prepare the DEFINES edge delete")?;

        // Deleted ids first, then upserted ones, each once. An id both deleted
        // and upserted ends up upserted (`apply_diff` deletes before it
        // upserts), which `final_membership` already says - but its edge is
        // still dropped here, because its row really is deleted in between:
        // on a connection enforcing foreign keys (this module's tests, half
        // of the time) that delete is refused while an edge still points at
        // it, and on one that does not (the daemon's - see the module doc) the
        // same code path is what removes the edge of a member deleted for
        // good, which nothing else would. [`attach`] puts the edge back for an
        // id that is re-upserted, and the recount never sees the gap.
        let deleted: HashSet<&str> = diff.delete_node_ids.iter().map(String::as_str).collect();
        let mut seen: HashSet<&str> = HashSet::new();
        let named = diff
            .delete_node_ids
            .iter()
            .map(String::as_str)
            .chain(diff.upsert_nodes.iter().map(|n| n.id.as_str()));
        for id in named {
            if !seen.insert(id) {
                continue;
            }
            let was: Option<Key> = current
                .query_row(params![id], |row| {
                    let language: String = row.get(0)?;
                    let container: Option<String> = row.get(1)?;
                    let native_kind: Option<String> = row.get(2)?;
                    Ok(membership(&language, container.as_deref(), native_kind.as_deref()))
                })
                .optional()
                .context("failed to read a node's current container")?
                .flatten();
            let Some(was) = was else { continue };
            let will_be = final_membership.get(id).cloned().flatten();
            if will_be.as_ref() != Some(&was) || deleted.contains(id) {
                drop_edge
                    .execute(params![defines_edge_id(&container_id(&was.0, &was.1), id)])
                    .context("failed to detach a member from its old container")?;
                touched.insert(was);
            }
        }
    }

    let mut members = Vec::new();
    let mut listed: HashSet<&str> = HashSet::new();
    for node in diff.upsert_nodes.iter().rev() {
        if !listed.insert(node.id.as_str()) {
            continue;
        }
        if let Some(Some(key)) = final_membership.get(node.id.as_str()) {
            touched.insert(key.clone());
            members.push((node.id.clone(), key.clone()));
        }
    }
    members.reverse();
    touched.extend(parents.iter().map(|(key, _)| key.clone()));

    Ok(Some(Pending { members, parents, touched }))
}

/// Phase two, run inside `apply_diff`'s transaction after every node and edge
/// write: attaches each member to its container (materializing the container
/// on first sight), applies `containerParent`, then recounts every touched
/// container and deletes the ones left empty.
pub(crate) fn attach(conn: &Connection, pending: Pending) -> Result<()> {
    let Pending { members, parents, touched } = pending;

    let mut created: HashSet<Key> = HashSet::new();
    {
        let mut add_edge = conn
            .prepare_cached(
                "INSERT INTO edges (id, fromId, toId, kind, source, engine, resolved, toDeclaration)
                 VALUES (?1, ?2, ?3, ?4, 'syntactic', ?5, 1, NULL)
                 ON CONFLICT(id) DO NOTHING",
            )
            .context("failed to prepare the DEFINES edge insert")?;
        for (member_id, key) in &members {
            let node_id = container_id(&key.0, &key.1);
            if ensure_container(conn, &node_id, key)? {
                created.insert(key.clone());
            }
            add_edge
                .execute(params![
                    defines_edge_id(&node_id, member_id),
                    node_id,
                    member_id,
                    DEFINES_KIND,
                    CORE_ENGINE
                ])
                .context("failed to attach a member to its container")?;
        }
    }

    apply_parents(conn, &parents, &created)?;

    let mut count = conn
        .prepare_cached("SELECT COUNT(*) FROM edges WHERE fromId = ?1 AND kind = ?2")
        .context("failed to prepare the member count")?;
    let mut store_count = conn
        .prepare_cached("UPDATE containers SET memberCount = ?2 WHERE nodeId = ?1")
        .context("failed to prepare the member count update")?;
    for key in touched {
        let node_id = container_id(&key.0, &key.1);
        let members: i64 = count
            .query_row(params![node_id, DEFINES_KIND], |row| row.get(0))
            .context("failed to count a container's members")?;
        if members == 0 {
            delete_container(conn, &node_id)?;
        } else {
            store_count
                .execute(params![node_id, members])
                .context("failed to store a container's member count")?;
        }
    }
    Ok(())
}

/// Writes the container node and its `containers` row if they are not there
/// yet. Returns whether the row was created by this call, which
/// [`apply_parents`] needs to tell a container's first parent from a change of
/// parent.
///
/// `memberCount` starts at 0 and `parentKey` at NULL: both are overwritten
/// before the transaction commits (the count by [`attach`]'s recount, the
/// parent by [`apply_parents`]), so neither initial value is ever observable.
fn ensure_container(conn: &Connection, node_id: &str, key: &Key) -> Result<bool> {
    let (language, container_key) = key;
    // `name` and `qualifiedName` are both the key. There is no language-neutral
    // rule for a key's last segment (`::`, `.` and `/` mean different things in
    // different languages - the design doc's reason for sending parents rather
    // than deriving them), and the key is the one string every language agrees
    // names the container. `visibility = 'public'`: a container is addressable
    // from anywhere; who may see its *members* is each member's own business.
    conn.prepare_cached(
        "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, visibility, language, nativeKind)
         VALUES (?1, ?2, ?3, ?3, '', 0, 0, 0, 0, 'public', ?4, ?5)
         ON CONFLICT(id) DO NOTHING",
    )
    .context("failed to prepare the container node insert")?
    .execute(params![node_id, MODULE_KIND, container_key, language, CONTAINER_NATIVE_KIND])
    .context("failed to materialize a container node")?;

    let inserted = conn
        .prepare_cached(
            "INSERT INTO containers (nodeId, language, key, parentKey, memberCount)
             VALUES (?1, ?2, ?3, NULL, 0)
             ON CONFLICT DO NOTHING",
        )
        .context("failed to prepare the containers row insert")?
        .execute(params![node_id, language, container_key])
        .context("failed to materialize a containers row")?;
    Ok(inserted == 1)
}

/// Applies each member record's `containerParent` to its container in diff
/// order, so the last record wins, and logs a container whose members
/// disagree. See the module doc for why this is not a rejection.
///
/// A container that ends the diff with no row (every member it was named by
/// has since left, and it had none before) is skipped: the `UPDATE` matches
/// nothing, and the recount after this deletes nothing either.
fn apply_parents(conn: &Connection, parents: &[(Key, Option<String>)], created: &HashSet<Key>) -> Result<()> {
    if parents.is_empty() {
        return Ok(());
    }
    let mut stored = conn
        .prepare_cached("SELECT parentKey FROM containers WHERE language = ?1 AND key = ?2")
        .context("failed to prepare the parent lookup")?;

    // `None`: nothing has named this container's parent yet - it has no row,
    // or its row was created by this very diff. `Some(p)`: the parent
    // currently in force, from the row or from an earlier record in this diff.
    let mut in_force: HashMap<&Key, Option<Option<String>>> = HashMap::new();
    let mut disagreeing: BTreeSet<&Key> = BTreeSet::new();
    for (key, parent) in parents {
        let current = match in_force.get(key) {
            Some(current) => current.clone(),
            None if created.contains(key) => None,
            None => stored
                .query_row(params![key.0, key.1], |row| row.get::<_, Option<String>>(0))
                .optional()
                .context("failed to read a container's parent")?,
        };
        if matches!(&current, Some(previous) if previous != parent) {
            disagreeing.insert(key);
        }
        in_force.insert(key, Some(parent.clone()));
    }

    for key in &disagreeing {
        crate::log_line!(
            "g-mesh: members of the {} container {:?} disagree about its parent (containerParent) - a \
             plugin bug; keeping the one sent last, {:?}",
            key.0,
            key.1,
            in_force.get(key).cloned().flatten().flatten(),
        );
    }

    let mut update = conn
        .prepare_cached("UPDATE containers SET parentKey = ?3 WHERE language = ?1 AND key = ?2")
        .context("failed to prepare the parent update")?;
    for (key, parent) in in_force {
        let parent = parent.flatten();
        update.execute(params![key.0, key.1, parent]).context("failed to store a container's parent")?;
    }
    Ok(())
}

/// Deletes an empty container and everything keyed by its id, explicitly,
/// since foreign keys are off on the daemon's connection. Edges first, both
/// directions, so the node delete is legal on a connection that does enforce
/// them. See the module doc for what happens to the importers whose edges go.
fn delete_container(conn: &Connection, node_id: &str) -> Result<()> {
    for sql in [
        "DELETE FROM edges WHERE fromId = ?1",
        "DELETE FROM edges WHERE toId = ?1",
        "DELETE FROM containers WHERE nodeId = ?1",
        // Neither ever exists for a container node today; deleted anyway,
        // for the reason `apply_diff` gives for its own node deletes - an
        // orphan here would be inherited by whatever next takes this id,
        // which for a content-derived id is this same container coming back.
        "DELETE FROM vectors WHERE nodeId = ?1",
        "DELETE FROM declarations WHERE nodeId = ?1",
        "DELETE FROM placeholder_targets WHERE nodeId = ?1",
        "DELETE FROM placeholder_also_scopes WHERE nodeId = ?1",
        "DELETE FROM qualified_suffixes WHERE nodeId = ?1",
        "DELETE FROM untyped_calls WHERE nodeId = ?1",
        "DELETE FROM nodes WHERE id = ?1",
    ] {
        conn.prepare_cached(sql)
            .context("failed to prepare an empty-container delete")?
            .execute(params![node_id])
            .context("failed to delete an empty container")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
