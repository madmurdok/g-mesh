//! Logical containers: the core-owned nodes a Go package, a Rust module, a C#
//! namespace... becomes in the graph (docs/architecture/multi-language-
//! plugins.md, "Data Model > Logical containers").
//!
//! A plugin never sends a container node. It sends *members* - ordinary
//! declarations carrying `container = <key>` (and `containerParent`) - and this
//! module materializes the rest inside [`storage::write::apply_diff`]'s own
//! transaction:
//!
//!  - the container node itself: a `Module` row with `nativeKind =
//!    'container'`, `filePath = ''` and the id [`container_id`] computes;
//!  - its `containers` row, carrying `parentKey` and `memberCount`;
//!  - one `DEFINES` edge from the container to each member;
//!  - and, when the last member goes, the deletion of all of the above plus
//!    every edge *into* the container.
//!
//! # Why core owns this and no diff does
//!
//! A container has members in many files, and a plugin's diff owns rows one
//! file at a time (Constraints: "Per-file ownership of rows"). No single file's
//! diff can say "this package now exists" or "this package is now empty",
//! because neither is a fact about that file. Worse, the diffs that *do*
//! arrive say less than they appear to: a plugin diffs against its previous
//! extraction, so a file re-sent after an edit carries only what changed. An
//! unchanged member is simply absent from that diff, and absence must not read
//! as departure. The only statements a diff makes about membership are the
//! ones it makes about the nodes it names - "this node is upserted with this
//! container" and "this node is deleted" - so those are the only ones read.
//!
//! # Decision: derive the count, never keep a running tally
//!
//! `memberCount` is not incremented and decremented as diffs say members come
//! and go. It is recomputed, for every container a diff touched, as the
//! number of `DEFINES` edges actually leaving that container node - the very
//! quantity the acceptance criterion compares it against. A running tally
//! would have to be right about intent on every path (a re-upsert that is not
//! a join, a delete of an id that was never a member, one id upserted twice in
//! one burst, a move whose old and new container are the same) and would stay
//! wrong forever after its first mistake. A count read back from the rows it
//! counts cannot drift from them: the most it can be is stale for a container
//! no diff touched, and an untouched container has had nothing change.
//!
//! The edges themselves are maintained per *member*, not per container, so a
//! diff costs work proportional to the nodes it names rather than to the size
//! of the containers they sit in (a C++ `llvm` namespace reopened in thousands
//! of files is the shape that rules out re-deriving a whole container's member
//! list on every edit):
//!
//!  1. [`detach`] runs first, before `apply_diff` deletes or upserts anything.
//!     For every node the diff names, it reads the membership the index holds
//!     for it *now* and compares it with the membership the diff leaves it with
//!     (the last upsert of that id, or none if the diff only deletes it). Where
//!     they differ, the old `DEFINES` edge is deleted. This must happen before
//!     the node delete: `edges.toId` is a foreign key, and in the test
//!     connections that enforce it a member cannot be deleted while an edge
//!     still points at it.
//!  2. [`attach`] runs last, once every node is written. Each member the diff
//!     leaves in a container gets its container node, its `containers` row and
//!     its `DEFINES` edge, each written `ON CONFLICT DO NOTHING` - so an
//!     already-member node re-upserted with the same container writes nothing
//!     and counts nothing twice. Then every touched container is recounted,
//!     and deleted at zero.
//!
//! **Cost of the recount.** `COUNT(*)` over one container's `DEFINES` edges
//! walks `idx_edges_fromId` for that container, so it is linear in the
//! container's size, once per diff that touches it: measured at 16 ms for a
//! 100,000-member container among 300,000 unrelated edges (SQLite 3.51, on a
//! machine at load average ~25), i.e. ~1.6 ms at 10,000 - larger than any Go
//! package or Rust module the design targets. If a future corpus (a C++
//! namespace reopened everywhere) makes that matter, the escape hatch is a
//! count adjusted by the rows each edge write *actually* changed (`execute`'s
//! return value), which keeps the no-intent property; it is not taken now
//! because the recount is the version that cannot drift even from a writer
//! that bypasses this module.
//!
//! A move is therefore exactly one detach from the old container and one
//! attach to the new one, however many times the id appears in the diff; a
//! swap of two members between two containers is two moves; and a container
//! that one diff both empties and refills is recounted once, at the end, and
//! never passes through zero at all.
//!
//! # Decision: inside `apply_diff`'s transaction, not a pass after it
//!
//! `graph::imports::link_diff`/`graph::symbol_links::link_diff` run after the
//! commit, and can afford to: an unlinked placeholder is a *correct* graph with
//! less in it, and the next pass finishes the job. Half-applied membership is
//! not correct - a committed member delete whose container still counts it, or
//! a new member with no container node, is exactly the drift this module exists
//! to rule out - so membership commits or rolls back with the diff that caused
//! it. It also covers every writer at once: the per-file reparse
//! (`watcher::apply`), each cold-start bulk batch (`daemon::bulk_index::commit`),
//! the burst batcher and `graph::queries`' helpers all write through
//! `apply_diff`, so none of them needs to know containers exist.
//!
//! **Bulk batch boundaries.** `daemon::bulk_index` may cut its stream anywhere,
//! including between two members of one container or between an id's first
//! and second record. Nothing here depends on where: membership is a function
//! of each node's last record, container existence of the edges that exist
//! after the batch, and `parentKey` of the last member record in stream order -
//! all three the same whether the stream arrived in one batch or twenty.
//!
//! # Who counts as a member
//!
//! Any node with a non-empty `container`, except a container node itself and
//! the four kinds that are an *address* rather than a declaration
//! (`pending_symbol`, `reexport`, `resolved_module`, `external_module`). Two
//! things break if a placeholder is counted: `graph::imports` deletes a
//! linked-away `resolved_module` placeholder with plain SQL (outside
//! `apply_diff`, so this module would never see it leave), and it only does
//! so once no edge touches the placeholder - which a `DEFINES` edge from its
//! container would prevent forever. An empty key is treated as no container
//! rather than as a container whose key is the empty string, which no
//! language in the design doc's table has.
//!
//! [`membership`] reads this exclusion off `graph::queries::
//! NON_DECLARATION_NATIVE_KINDS` (GM-376) - the same list `find_by_name`,
//! `find_by_qualified_name`, `find_by_position` and `mcp::find_definition`'s
//! candidate page consult, and the one `graph::symbol_links::is_declaration`
//! has read since GM-372 - rather than spelling its own copy. It had been a
//! four-kind copy missing `external_module` until now, which
//! `graph::symbol_links::requesters_below_new_containers`' own comment names
//! as the coupling to watch: that function compares its upserted-member count
//! against the `containers.memberCount` this module maintains, so the two
//! must exclude the same kinds. They agreed on everything any plugin actually
//! sends only because an import record carries no container at all, so it is
//! filtered out one line above by the `is_empty` check regardless of which
//! list is consulted - see `graph::containers::tests::
//! placeholders_and_empty_keys_are_never_members` for the case that only a
//! hand-built node (not a shipped plugin) can reach, where the two lists used
//! to disagree.
//!
//! # `parentKey`: last writer wins, and a disagreement is logged
//!
//! `containerParent` arrives on members, not on the container, so two members
//! of one container can disagree about it. That is always a plugin bug - no
//! language gives one container two parents - and the choice is between
//! rejecting the diff and keeping one answer. Rejecting would fail a whole
//! file's reparse (or a whole bulk batch) over a field only the visibility
//! check reads, which is the wrong trade for a graph that is otherwise fine,
//! so the last member record written wins, in diff order, and the
//! disagreement is logged once per container per diff. `None` is a value like
//! any other here ("this container is a root"), not "unknown": that is what
//! keeps the result a function of stream order alone. Treating `None` as
//! "leave it" would make the stored parent depend on whether a container
//! happened to be garbage-collected in between, i.e. on history rather than
//! input.
//!
//! # Deleting an empty container, with `foreign_keys` off
//!
//! The daemon's connection does not enforce foreign keys -
//! `storage::connection::open` switches them off explicitly, because the
//! bundled SQLite defaults them *on* (GM-292, fixed by GM-293/GM-294; until
//! then this paragraph described the intended state, not the real one) - so
//! `containers.nodeId`'s `ON DELETE CASCADE` never fires in production. This
//! module's tests run both pragma states for exactly that reason. Every row
//! is deleted explicitly instead: the
//! container's outgoing edges, its incoming edges (the `File -IMPORTS->
//! container` edges GM-267 will add), its `containers` row, any child rows keyed
//! by its id, and the node. The incoming edges' owners are not told: an
//! importer whose `IMPORTS` edge pointed at a now-deleted package keeps no edge
//! at all until that importer is reindexed, when its plugin re-sends the
//! placeholder and the linker tries again. That is deliberately the same
//! non-eager behaviour `graph::imports` documents for a deleted target file
//! ("Not covered, deliberately: a file *deleted* from the project"), for the
//! same reason: re-placeholdering eagerly would mean core inventing a
//! plugin-shaped node it has no content for.
//!
//! # Queries
//!
//! A container node has no source: `filePath` is `''`, its range is zero, and
//! nothing a file-anchored tool reads (a snippet, a staleness check, an
//! outline) means anything for it. `graph::queries`' name, qualifiedName and
//! position lookups, and `find_definition`'s candidate ranking, exclude it for
//! the same reason they exclude placeholders - answering "where is `server`
//! defined" with a row that points at no file sends the caller nowhere. The
//! other tools cannot reach one: `find_references`/`find_callers`/
//! `find_callees`/`find_implementations` walk `CALLS`/`REFERENCES`/
//! `SUPERTYPE_OF` edges and a container has only `DEFINES` ones;
//! `get_file_outline` pages `DEFINES` out of a `File` node, never into a
//! member; `get_dependencies` walks `IMPORTS`, which nothing points at a
//! container yet; and `search_code` only ranks nodes that have a `vectors` row,
//! which a container never gets (`EmbeddingPipeline::apply` embeds a diff's
//! `upsert_nodes`, and container nodes are never in one - and would embed
//! nothing anyway, having no signature or doc comment).

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::graph::queries::NON_DECLARATION_NATIVE_KINDS;
use crate::storage::write::{Diff, NodeRecord};

/// The `nativeKind` of a core-materialized container node. Never plugin-emitted
/// - `protocol::conformance` reports one on the wire as a violation.
pub const CONTAINER_NATIVE_KIND: &str = "container";

const MODULE_KIND: &str = "Module";
const DEFINES_KIND: &str = "DEFINES";
/// Only [`defining_containers`] needs this, to find the file node whose span
/// says which of a file's own declarations *is* the file.
const FILE_KIND: &str = "File";

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

/// The id of the container node for `key` in `language`.
///
/// Same family as every plugin-computed id (`hash` in
/// plugins/typescript/src/extract.ts): sha256 over a type-marked string,
/// lowercase hex, truncated to 32 characters. The preimage is
/// `"container " + language + "\0" + key`:
///
///  - The `container ` marker is what keeps it disjoint from plugin ids, whose
///    preimages start `node ` (`nodeIdFor`) or `edge ` (`edgeIdFor`). Two ids
///    can then only coincide by a collision in 128 bits of sha256, not by any
///    choice of path, name or key.
///  - NUL, not a space, between language and key, because a key is free text
///    and may contain spaces (a C++ namespace never does, but nothing here
///    should rely on which languages exist). `language` is a plugin directory
///    name (`daemon::manifest` requires the two to match) and no filesystem
///    allows NUL in one, so the split is unambiguous.
///
/// A plugin that ever needs to address a container node directly computes the
/// same string; the design doc's "plugins and core compute the same id" is this
/// function.
pub fn container_id(language: &str, key: &str) -> String {
    let digest = Sha256::digest(format!("container {language}\0{key}").as_bytes());
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// The id of the `DEFINES` edge from `container_node_id` to `member_id`,
/// computed exactly as `edgeIdFor(from, "DEFINES", to)` in extract.ts would.
/// Reusing the plugin's own edge scheme is safe because the `from` half is a
/// container id no plugin node can have (see [`container_id`]), and it means
/// the edge is the one any tier would name for that triple.
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
        eprintln!(
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
        "DELETE FROM nodes WHERE id = ?1",
    ] {
        conn.prepare_cached(sql)
            .context("failed to prepare an empty-container delete")?
            .execute(params![node_id])
            .context("failed to delete an empty container")?;
    }
    Ok(())
}

/// The ancestors of container `key` in `language`, nearest first, not including
/// `key` itself - the walk the visibility check (design doc: Interfaces >
/// Linker contract, step 5) needs for "is `c` `fromContainer` or one of its
/// ancestors": `from == c || parent_chain(conn, language, from)?.contains(c)`.
///
/// Walks `containers.parentKey`, which exists only for containers that have at
/// least one member. So the chain has **gaps** wherever an ancestor has no
/// members of its own - a Java package `com.acme` holding only subpackages, or
/// a Rust module with no items - and it handles them this way:
///
///  - The gap key itself *is* returned. It was named as a parent by the row
///    below it, which is a fact regardless of whether it has members.
///  - The walk stops there. What lies above a memberless container is not
///    recorded anywhere core can read, and core does not split keys to guess
///    (`::`, `.` and `/` are language rules, not core's).
///
/// So an ancestor is only ever reported when it is known to be one, never
/// guessed - a gap can make the visibility check refuse a link that a complete
/// chain would allow (a missing edge), never allow one it should refuse (a
/// wrong edge). How often that bites is up to each plugin: Go and Java/Kotlin
/// packages are flat (no parents at all), and a Rust plugin closes every gap
/// the language could open by emitting each `mod child;` item as a member of
/// the module that declares it - a module with submodules then always has a
/// member, and `pub(crate)`/`pub(super)` always find their ancestor. That is a
/// requirement on the Rust plugin (the design doc's Rollout, R3), recorded here because this is
/// where it bites.
///
/// A key with no row at all yields an empty chain. A cycle - a plugin bug, two
/// containers each naming the other as parent - ends the walk at the first
/// repeated key rather than looping.
pub fn parent_chain(conn: &Connection, language: &str, key: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare_cached("SELECT parentKey FROM containers WHERE language = ?1 AND key = ?2")
        .context("failed to prepare the parent-chain lookup")?;
    let mut chain = Vec::new();
    let mut seen: HashSet<String> = HashSet::from([key.to_string()]);
    let mut current = key.to_string();
    loop {
        let parent: Option<String> = stmt
            .query_row(params![language, current], |row| row.get(0))
            .optional()
            .context("failed to read a container's parent")?
            .flatten();
        let Some(parent) = parent else { break };
        if !seen.insert(parent.clone()) {
            break;
        }
        chain.push(parent.clone());
        current = parent;
    }
    Ok(chain)
}

/// The container a file defines, as [`defining_containers`] reports it.
pub struct DefiningContainer {
    /// The container's key - `requests.adapters`, `grep_searcher::sink`,
    /// `github.com/gin-gonic/gin/render`. The anchor a caller would pass to
    /// reach this container directly.
    pub key: String,
    /// The container's own node id, i.e. what a walk anchors on.
    pub node_id: String,
    pub language: String,
}

/// The container(s) `file_path` *is* - the module a Go, Rust or Python file
/// defines, as opposed to the modules it merely imports.
///
/// WHY THIS EXISTS (GM-356)
///
/// Outside TypeScript the import graph's nodes are containers, not files: an
/// `IMPORTS` edge leaves a `File` node and arrives at a container. So an
/// `Incoming` walk anchored on a file completes having found nothing, and
/// reports the well-formed zero that means "nothing imports this" about a
/// file that is imported four times over. `mcp::get_dependencies::from_file`
/// uses this to walk from the container instead, and says so.
///
/// HOW A FILE'S OWN CONTAINER IS TOLD FROM THE ONES IT ONLY MENTIONS
///
/// The index records membership one way only - `DEFINES` edges from a
/// container to the declarations in it (see this module's head comment) - so
/// "the container this file defines" has to be read back off the file's
/// declarations. Two shapes make that more than a `DISTINCT`:
///
///  1. **A file's own module node is a member of its *parent*.** The Python
///     plugin emits one `Module` declaration per file (`adapters` in
///     `requests/adapters.py`), and its `container` is `requests`, not
///     `requests.adapters` - because that is where the module is *declared*.
///     Counting it would offer the parent package as a candidate for every
///     file in it. It is excluded by the one thing that distinguishes it from
///     an ordinary member: it spans the whole file. A `mod sinks { .. }`
///     nested inside `sink.rs` spans lines 516-662 of 663 and is kept, which
///     is right - it is declared *in* `grep_searcher::sink` and so evidences
///     it.
///  2. **A file can also declare containers below its own.** That same nested
///     `mod sinks` gives its own container members in this file, so
///     `grep_searcher::sink::sinks` is a candidate too. Any candidate with
///     another candidate in its [`parent_chain`] is dropped: a module
///     declared inside a file is not the module that file *is*.
///
/// What is left is normally exactly one container, and the caller is expected
/// to treat anything else as unanswerable rather than pick. Measured over
/// three indexed repositories - gin, ripgrep and requests - every one of the
/// 234 files with any candidate at all resolved to exactly one, and it was
/// the right one: all 97 Go files matched the import path derived
/// independently from `go.mod` plus the file's directory, and all 18 Python
/// files inside the package matched the module path derived from theirs (the
/// other 16 are outside any package and correctly get the plugin's own
/// `orphan:<path>` container).
///
/// An empty result is the honest answer for a file that declares nothing the
/// index carries a container for - a `doc.go`, a `setup.py`, a Rust
/// integration-test binary. There is no module to name, so the caller has
/// nothing to substitute and nothing to suggest.
///
/// Keyed throughout: the file's nodes by `idx_nodes_filePath`, their
/// containers by `idx_edges_toId` and `containers`' primary key.
pub fn defining_containers(conn: &Connection, file_path: &str) -> Result<Vec<DefiningContainer>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT DISTINCT ct.key, ct.nodeId, ct.language \
             FROM nodes m \
             JOIN edges e ON e.toId = m.id AND e.kind = ?2 \
             JOIN containers ct ON ct.nodeId = e.fromId \
             JOIN nodes f ON f.kind = ?3 AND f.filePath = ?1 \
             WHERE m.filePath = ?1 \
               AND NOT (m.kind = ?4 AND m.startLine <= f.startLine AND m.endLine >= f.endLine) \
             ORDER BY ct.key",
        )
        .context("failed to prepare the defining-container lookup")?;
    let rows = stmt
        .query_map(params![file_path, DEFINES_KIND, FILE_KIND, MODULE_KIND], |row| {
            Ok(DefiningContainer { key: row.get(0)?, node_id: row.get(1)?, language: row.get(2)? })
        })
        .context("failed to look up a file's containers")?;
    let candidates: Vec<DefiningContainer> =
        rows.collect::<rusqlite::Result<_>>().context("failed to read a file's containers")?;

    // One candidate cannot be below another, so the parent walk is skipped
    // entirely for the overwhelmingly common case.
    if candidates.len() < 2 {
        return Ok(candidates);
    }
    // `(language, key)` throughout, never `key` alone: a key is only unique
    // within a language (`containers`' own `UNIQUE (language, key)`), and
    // nothing here needs to assume one file produced candidates in only one.
    let keys: Vec<(String, String)> =
        candidates.iter().map(|c| (c.language.clone(), c.key.clone())).collect();
    let mut outermost = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let chain = parent_chain(conn, &candidate.language, &candidate.key)?;
        let below_another =
            keys.iter().any(|(language, key)| *language == candidate.language && chain.contains(key));
        if !below_another {
            outermost.push(candidate);
        }
    }
    Ok(outermost)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::graph::imports::{EXTERNAL_MODULE_NATIVE_KIND, RESOLVED_MODULE_NATIVE_KIND};
    use crate::graph::symbol_links::{PENDING_SYMBOL_NATIVE_KIND, REEXPORT_NATIVE_KIND};
    use crate::storage::schema;
    use crate::storage::write::{apply_diff, EdgeRecord};

    /// Both connection shapes this module has to be right on: `foreign_keys`
    /// on, which is how every other storage test runs and which turns a
    /// dangling edge into an error, and off, which is how the daemon actually
    /// runs and where nothing cascades - so an explicit delete missed here
    /// leaves an orphan behind instead of failing.
    fn setup(foreign_keys: bool) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", if foreign_keys { "ON" } else { "OFF" }).unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn member(id: &str, language: &str, container: Option<&str>, parent: Option<&str>) -> NodeRecord {
        let mut node = NodeRecord::new(id, "Function", id, id, format!("src/{id}.x"), language);
        node.container = container.map(str::to_string);
        node.container_parent = parent.map(str::to_string);
        node
    }

    fn upsert(nodes: Vec<NodeRecord>) -> Diff {
        Diff { upsert_nodes: nodes, ..Default::default() }
    }

    fn delete(ids: &[&str]) -> Diff {
        Diff { delete_node_ids: ids.iter().map(|id| id.to_string()).collect(), ..Default::default() }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Row {
        language: String,
        key: String,
        node_id: String,
        parent_key: Option<String>,
        member_count: i64,
    }

    fn rows(conn: &Connection) -> Vec<Row> {
        let mut stmt = conn
            .prepare_cached(
                "SELECT language, key, nodeId, parentKey, memberCount FROM containers ORDER BY language, key",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok(Row {
                language: row.get(0)?,
                key: row.get(1)?,
                node_id: row.get(2)?,
                parent_key: row.get(3)?,
                member_count: row.get(4)?,
            })
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    fn row(conn: &Connection, language: &str, key: &str) -> Option<Row> {
        rows(conn).into_iter().find(|row| row.language == language && row.key == key)
    }

    /// The members a container's `DEFINES` edges reach, sorted.
    fn edge_members(conn: &Connection, language: &str, key: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare_cached("SELECT toId FROM edges WHERE fromId = ?1 AND kind = 'DEFINES' ORDER BY toId")
            .unwrap();
        stmt.query_map(params![container_id(language, key)], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.prepare_cached(sql).unwrap().query_row([], |row| row.get(0)).unwrap()
    }

    /// Every invariant that holds after *any* sequence of diffs, checked
    /// against the tables alone - no model of what the diffs meant. `Err`
    /// names the first one broken.
    fn check_invariants(conn: &Connection) -> Result<(), String> {
        for row in rows(conn) {
            if row.node_id != container_id(&row.language, &row.key) {
                return Err(format!("{row:?}: nodeId is not container_id(language, key)"));
            }
            let node: Option<(String, Option<String>, String, String, String, i64)> = conn
                .query_row(
                    "SELECT kind, nativeKind, filePath, language, name, exported FROM nodes WHERE id = ?1",
                    params![row.node_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
                )
                .optional()
                .unwrap();
            let expected = (
                MODULE_KIND.to_string(),
                Some(CONTAINER_NATIVE_KIND.to_string()),
                String::new(),
                row.language.clone(),
                row.key.clone(),
                1,
            );
            if node.as_ref() != Some(&expected) {
                return Err(format!("{row:?}: container node is {node:?}, expected {expected:?}"));
            }
            let defines = edge_members(conn, &row.language, &row.key).len() as i64;
            if row.member_count != defines {
                return Err(format!("{row:?}: memberCount != {defines} DEFINES edges"));
            }
            if row.member_count == 0 {
                return Err(format!("{row:?}: a container with zero members exists"));
            }
        }

        let orphan_nodes = count(
            conn,
            "SELECT COUNT(*) FROM nodes n WHERE n.nativeKind = 'container' \
             AND NOT EXISTS (SELECT 1 FROM containers c WHERE c.nodeId = n.id)",
        );
        if orphan_nodes != 0 {
            return Err(format!("{orphan_nodes} container node(s) without a containers row"));
        }

        // Every DEFINES edge out of a container lands on a node that really is
        // a member of exactly that container, under the id core computes...
        let mut stmt = conn
            .prepare_cached(
                "SELECT e.id, c.language, c.key, e.toId, n.language, n.container, n.nativeKind \
                 FROM edges e JOIN containers c ON c.nodeId = e.fromId LEFT JOIN nodes n ON n.id = e.toId",
            )
            .unwrap();
        /// `(edge id, container language, container key, member id, and the
        /// member's own language/container/nativeKind - NULL if it is gone)`.
        type ContainerEdge = (String, String, String, String, Option<String>, Option<String>, Option<String>);
        let edges: Vec<ContainerEdge> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let mut attached: HashSet<(String, Key)> = HashSet::new();
        for (edge_id, language, key, to_id, n_language, n_container, n_native_kind) in edges {
            attached.insert((to_id.clone(), (language.clone(), key.clone())));
            let Some(n_language) = n_language else {
                return Err(format!(
                    "DEFINES edge {edge_id} from {language}/{key} points at missing node {to_id}"
                ));
            };
            let actual = membership(&n_language, n_container.as_deref(), n_native_kind.as_deref());
            if actual != Some((language.clone(), key.clone())) {
                return Err(format!(
                    "edge from {language}/{key} reaches {to_id}, whose membership is {actual:?}"
                ));
            }
            if edge_id != defines_edge_id(&container_id(&language, &key), &to_id) {
                return Err(format!("edge {edge_id} into {to_id} does not carry the computed id"));
            }
        }

        // ...and every member has one.
        let mut stmt = conn.prepare_cached("SELECT id, language, container, nativeKind FROM nodes").unwrap();
        let nodes: Vec<(String, String, Option<String>, Option<String>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for (id, language, container, native_kind) in nodes {
            let Some(key) = membership(&language, container.as_deref(), native_kind.as_deref()) else {
                continue;
            };
            if !attached.contains(&(id.clone(), key.clone())) {
                return Err(format!("member {id} of {key:?} has no DEFINES edge from a live container"));
            }
        }

        let dangling = count(
            conn,
            "SELECT COUNT(*) FROM edges e WHERE NOT EXISTS (SELECT 1 FROM nodes n WHERE n.id = e.fromId) \
             OR NOT EXISTS (SELECT 1 FROM nodes n WHERE n.id = e.toId)",
        );
        if dangling != 0 {
            return Err(format!("{dangling} edge(s) with a missing endpoint"));
        }
        Ok(())
    }

    #[test]
    fn a_first_member_materializes_its_container() {
        let mut conn = setup(true);
        apply_diff(&mut conn, &upsert(vec![member("a", "go", Some("github.com/x/app"), None)])).unwrap();

        assert_eq!(
            rows(&conn),
            vec![Row {
                language: "go".to_string(),
                key: "github.com/x/app".to_string(),
                node_id: container_id("go", "github.com/x/app"),
                parent_key: None,
                member_count: 1,
            }]
        );
        assert_eq!(edge_members(&conn, "go", "github.com/x/app"), vec!["a"]);
        let (source, engine, resolved): (String, String, bool) = conn
            .query_row("SELECT source, engine, resolved FROM edges WHERE kind = 'DEFINES'", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!((source.as_str(), engine.as_str(), resolved), ("syntactic", CORE_ENGINE, true));
        check_invariants(&conn).unwrap();
    }

    /// Idempotency, the acceptance criterion's first trap: an already-member
    /// node upserted again with the same container - which every reparse that
    /// touches it does - must not count it twice.
    #[test]
    fn re_upserting_a_member_does_not_count_it_again() {
        let mut conn = setup(true);
        for _ in 0..3 {
            apply_diff(&mut conn, &upsert(vec![member("a", "go", Some("pkg"), None)])).unwrap();
        }
        assert_eq!(row(&conn, "go", "pkg").unwrap().member_count, 1);
        check_invariants(&conn).unwrap();
    }

    /// The second trap: a plugin re-sends a file with only what changed, so an
    /// unchanged member is absent from the diff - and absent is not deleted.
    #[test]
    fn a_file_re_sent_without_an_unchanged_member_keeps_it() {
        let mut conn = setup(true);
        apply_diff(
            &mut conn,
            &upsert(vec![member("a", "go", Some("pkg"), None), member("b", "go", Some("pkg"), None)]),
        )
        .unwrap();

        let mut changed = member("a", "go", Some("pkg"), None);
        changed.signature = Some("func a(x int)".to_string());
        apply_diff(&mut conn, &upsert(vec![changed])).unwrap();

        assert_eq!(row(&conn, "go", "pkg").unwrap().member_count, 2);
        assert_eq!(edge_members(&conn, "go", "pkg"), vec!["a", "b"]);
        check_invariants(&conn).unwrap();
    }

    #[test]
    fn a_move_leaves_the_old_container_and_joins_the_new_one_in_the_same_diff() {
        let mut conn = setup(true);
        apply_diff(
            &mut conn,
            &upsert(vec![member("a", "go", Some("old"), None), member("b", "go", Some("old"), None)]),
        )
        .unwrap();

        apply_diff(&mut conn, &upsert(vec![member("a", "go", Some("new"), None)])).unwrap();

        assert_eq!(row(&conn, "go", "old").unwrap().member_count, 1);
        assert_eq!(row(&conn, "go", "new").unwrap().member_count, 1);
        assert_eq!(edge_members(&conn, "go", "old"), vec!["b"]);
        assert_eq!(edge_members(&conn, "go", "new"), vec!["a"]);
        check_invariants(&conn).unwrap();
    }

    /// Moves "counted exactly once": a swap is two moves in one diff, and the
    /// same id upserted twice in one diff (a burst) is one move, to wherever
    /// its last record puts it.
    #[test]
    fn swaps_and_repeated_records_in_one_diff_count_each_member_once() {
        let mut conn = setup(true);
        apply_diff(
            &mut conn,
            &upsert(vec![member("a", "go", Some("left"), None), member("b", "go", Some("right"), None)]),
        )
        .unwrap();

        apply_diff(
            &mut conn,
            &upsert(vec![
                member("a", "go", Some("right"), None),
                member("b", "go", Some("left"), None),
                member("b", "go", Some("elsewhere"), None),
                member("b", "go", Some("left"), None),
            ]),
        )
        .unwrap();

        assert_eq!(edge_members(&conn, "go", "left"), vec!["b"]);
        assert_eq!(edge_members(&conn, "go", "right"), vec!["a"]);
        assert_eq!(row(&conn, "go", "elsewhere"), None, "a container only a superseded record named");
        assert_eq!(row(&conn, "go", "left").unwrap().member_count, 1);
        assert_eq!(row(&conn, "go", "right").unwrap().member_count, 1);
        check_invariants(&conn).unwrap();
    }

    /// A container that one diff empties and refills never reaches zero, so
    /// it is never deleted - and so nothing pointing at it loses its edge.
    #[test]
    fn a_container_emptied_and_refilled_in_one_diff_survives_with_its_incoming_edges() {
        for foreign_keys in [true, false] {
            let mut conn = setup(foreign_keys);
            apply_diff(&mut conn, &upsert(vec![member("a", "go", Some("pkg"), None)])).unwrap();
            let importer = importer_of(&mut conn, "go", "pkg");

            let mut diff = delete(&["a"]);
            diff.upsert_nodes.push(member("b", "go", Some("pkg"), None));
            apply_diff(&mut conn, &diff).unwrap();

            assert_eq!(edge_members(&conn, "go", "pkg"), vec!["b"]);
            assert_eq!(count(&conn, &format!("SELECT COUNT(*) FROM edges WHERE id = '{importer}'")), 1);
            check_invariants(&conn).unwrap();
        }
    }

    /// Stands in for GM-267's `File -IMPORTS-> container` edge: a file node
    /// and an edge into the container node, written the way any diff writes
    /// them. Returns the edge id.
    fn importer_of(conn: &mut Connection, language: &str, key: &str) -> String {
        let file = NodeRecord::new("importer", "File", "main.go", "main.go", "main.go", language);
        let edge = EdgeRecord::new(
            "imports-pkg",
            "importer",
            container_id(language, key),
            "IMPORTS",
            "syntactic",
            true,
        );
        apply_diff(conn, &Diff { upsert_nodes: vec![file], upsert_edges: vec![edge], ..Default::default() })
            .unwrap();
        "imports-pkg".to_string()
    }

    #[test]
    fn deleting_the_last_member_deletes_the_container_and_every_edge_into_it() {
        for foreign_keys in [true, false] {
            let mut conn = setup(foreign_keys);
            apply_diff(
                &mut conn,
                &upsert(vec![member("a", "go", Some("pkg"), None), member("b", "go", Some("pkg"), None)]),
            )
            .unwrap();
            let importer = importer_of(&mut conn, "go", "pkg");

            apply_diff(&mut conn, &delete(&["a"])).unwrap();
            assert_eq!(row(&conn, "go", "pkg").unwrap().member_count, 1, "fk={foreign_keys}");

            apply_diff(&mut conn, &delete(&["b"])).unwrap();
            assert_eq!(rows(&conn), vec![], "fk={foreign_keys}");
            let node_id = container_id("go", "pkg");
            assert_eq!(count(&conn, &format!("SELECT COUNT(*) FROM nodes WHERE id = '{node_id}'")), 0);
            assert_eq!(
                count(
                    &conn,
                    &format!("SELECT COUNT(*) FROM edges WHERE fromId = '{node_id}' OR toId = '{node_id}'")
                ),
                0,
                "fk={foreign_keys}: DEFINES out of it and {importer} into it must both go"
            );
            assert_eq!(
                count(&conn, "SELECT COUNT(*) FROM nodes WHERE id = 'importer'"),
                1,
                "the importer stays"
            );
            check_invariants(&conn).unwrap();
        }
    }

    /// A placeholder is an address, not a declaration: counting one would
    /// keep a linked-away `resolved_module` alive forever (its `DEFINES`
    /// edge is an incident edge `graph::imports` waits to see gone).
    ///
    /// `external_module` is in this list too (GM-376) even though no shipped
    /// plugin ever sends one with a non-empty `container` - the case is only
    /// reachable the way this test reaches it, by constructing the
    /// `NodeRecord` directly rather than through a plugin's wire output. It
    /// is the arm that fails without GM-376's change to [`membership`]: on
    /// the four-kind copy `membership` used to spell, an `external_module`
    /// row here was still counted, so `containers.memberCount` would disagree
    /// with `graph::symbol_links::requesters_below_new_containers`'s own
    /// count for it - the coupling that function's own comment names. The
    /// other four kinds fail without any filter at all, so they are not the
    /// control for this change; see
    /// `a_module_that_is_not_an_address_is_still_a_member` for that.
    #[test]
    fn placeholders_and_empty_keys_are_never_members() {
        let mut conn = setup(true);
        let mut nodes = Vec::new();
        for native_kind in [
            PENDING_SYMBOL_NATIVE_KIND,
            REEXPORT_NATIVE_KIND,
            RESOLVED_MODULE_NATIVE_KIND,
            EXTERNAL_MODULE_NATIVE_KIND,
        ] {
            let mut placeholder = member(native_kind, "go", Some("pkg"), None);
            placeholder.kind = MODULE_KIND.to_string();
            placeholder.native_kind = Some(native_kind.to_string());
            nodes.push(placeholder);
        }
        nodes.push(member("empty", "go", Some(""), None));
        apply_diff(&mut conn, &upsert(nodes)).unwrap();

        assert_eq!(rows(&conn), vec![]);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM edges"), 0);
        check_invariants(&conn).unwrap();
    }

    /// The control for the test above: a `Module`-kind node that is not one
    /// of the excluded native kinds - a TypeScript namespace is the real
    /// example (`plugins/typescript/src/extract.ts` emits `kind: "Module",
    /// nativeKind: "namespace"`) - is still a member, so the exclusion above
    /// is about `nativeKind`, not about `kind == "Module"`.
    #[test]
    fn a_module_that_is_not_an_address_is_still_a_member() {
        let mut conn = setup(true);
        let mut namespace = member("ns", "typescript", Some("pkg"), None);
        namespace.kind = MODULE_KIND.to_string();
        namespace.native_kind = Some("namespace".to_string());
        apply_diff(&mut conn, &upsert(vec![namespace])).unwrap();

        assert_eq!(row(&conn, "typescript", "pkg").unwrap().member_count, 1);
        check_invariants(&conn).unwrap();
    }

    #[test]
    fn the_same_key_in_two_languages_is_two_containers() {
        let mut conn = setup(true);
        apply_diff(
            &mut conn,
            &upsert(vec![member("a", "go", Some("shared"), None), member("b", "rust", Some("shared"), None)]),
        )
        .unwrap();
        assert_ne!(container_id("go", "shared"), container_id("rust", "shared"));
        assert_eq!(rows(&conn).len(), 2);
        apply_diff(&mut conn, &delete(&["a"])).unwrap();
        assert_eq!(row(&conn, "go", "shared"), None);
        assert_eq!(row(&conn, "rust", "shared").unwrap().member_count, 1);
        check_invariants(&conn).unwrap();
    }

    /// The id is read back from the documented preimage rather than written
    /// down as a constant, so this checks the scheme, not a copy of its output.
    #[test]
    fn container_ids_hash_a_marked_preimage_in_the_plugin_id_family() {
        let expected: String = Sha256::digest(b"container go\0github.com/x/app")
            .iter()
            .take(16)
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(container_id("go", "github.com/x/app"), expected);
        assert_eq!(expected.len(), 32, "same truncation as extract.ts's hash()");
        // The NUL separator is what keeps these apart; a space would not.
        assert_ne!(container_id("a b", "c"), container_id("a", "b c"));
        // The marker is what keeps a container id off every plugin node id:
        // extract.ts hashes `node <path> <kind> <qualifiedName> <nativeKind>`.
        let plugin_style: String = Sha256::digest(b"node  Module github.com/x/app container")
            .iter()
            .take(16)
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_ne!(container_id("go", "github.com/x/app"), plugin_style);
    }

    /// TS sends no container, and its behaviour must not change: no rows, no
    /// edges, no nodes beyond the diff's own, and the fast path taken.
    #[test]
    fn a_diff_with_no_containers_writes_nothing_extra_and_skips_the_hook() {
        let mut conn = setup(true);
        let diff = Diff {
            upsert_nodes: vec![
                NodeRecord::new("f", "File", "a.ts", "a.ts", "a.ts", "typescript"),
                NodeRecord::new("n", "Function", "run", "run", "a.ts", "typescript"),
            ],
            upsert_edges: vec![EdgeRecord::new("e", "f", "n", "DEFINES", "tree-sitter", false)],
            ..Default::default()
        };
        assert!(detach(&conn, &diff).unwrap().is_none());
        apply_diff(&mut conn, &diff).unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM nodes"), 2);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM edges"), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM containers"), 0);
        assert!(detach(&conn, &delete(&["n"])).unwrap().is_none());
    }

    #[test]
    fn conflicting_parents_keep_the_last_one_sent() {
        let mut conn = setup(true);
        apply_diff(
            &mut conn,
            &upsert(vec![
                member("a", "rust", Some("c::m"), Some("c")),
                member("b", "rust", Some("c::m"), Some("bogus")),
            ]),
        )
        .unwrap();
        assert_eq!(row(&conn, "rust", "c::m").unwrap().parent_key.as_deref(), Some("bogus"));

        apply_diff(&mut conn, &upsert(vec![member("a", "rust", Some("c::m"), Some("c"))])).unwrap();
        assert_eq!(row(&conn, "rust", "c::m").unwrap().parent_key.as_deref(), Some("c"));

        // `None` is a value too: the container is now recorded as a root.
        apply_diff(&mut conn, &upsert(vec![member("b", "rust", Some("c::m"), None)])).unwrap();
        assert_eq!(row(&conn, "rust", "c::m").unwrap().parent_key, None);
        check_invariants(&conn).unwrap();
    }

    #[test]
    fn parent_chain_walks_to_the_root_nearest_first() {
        let mut conn = setup(true);
        apply_diff(
            &mut conn,
            &upsert(vec![
                member("root", "rust", Some("krate"), None),
                member("mid", "rust", Some("krate::a"), Some("krate")),
                member("leaf", "rust", Some("krate::a::b"), Some("krate::a")),
                // Same keys in another language must not leak into the walk.
                member("go", "go", Some("krate::a"), Some("elsewhere")),
            ]),
        )
        .unwrap();

        assert_eq!(parent_chain(&conn, "rust", "krate::a::b").unwrap(), vec!["krate::a", "krate"]);
        assert_eq!(parent_chain(&conn, "rust", "krate").unwrap(), Vec::<String>::new());
        assert_eq!(parent_chain(&conn, "rust", "never-seen").unwrap(), Vec::<String>::new());
    }

    /// An ancestor with no members has no row, so nothing records *its*
    /// parent: the walk names the gap (the row below it said so) and stops,
    /// rather than guessing by splitting the key.
    #[test]
    fn parent_chain_names_a_memberless_ancestor_and_stops_there() {
        let mut conn = setup(true);
        apply_diff(
            &mut conn,
            &upsert(vec![
                member("root", "rust", Some("krate"), None),
                member("leaf", "rust", Some("krate::a::b"), Some("krate::a")),
            ]),
        )
        .unwrap();
        assert_eq!(parent_chain(&conn, "rust", "krate::a::b").unwrap(), vec!["krate::a"]);

        // The gap closes as soon as the ancestor gains a member.
        apply_diff(&mut conn, &upsert(vec![member("mid", "rust", Some("krate::a"), Some("krate"))])).unwrap();
        assert_eq!(parent_chain(&conn, "rust", "krate::a::b").unwrap(), vec!["krate::a", "krate"]);
    }

    #[test]
    fn parent_chain_ends_at_a_cycle() {
        let mut conn = setup(true);
        apply_diff(
            &mut conn,
            &upsert(vec![
                member("x", "rust", Some("x"), Some("y")),
                member("y", "rust", Some("y"), Some("x")),
            ]),
        )
        .unwrap();
        assert_eq!(parent_chain(&conn, "rust", "x").unwrap(), vec!["y"]);
    }

    /// The daemon's own write path: a real `FileChangeDiff`-shaped wire node
    /// goes through `watcher::apply::to_node_record`, which must carry
    /// `containerParent` through rather than drop it as it did before GM-265.
    #[test]
    fn the_wire_conversion_carries_container_parent_through_to_the_row() {
        use crate::protocol::types::{NodeKind, Position, Range, Visibility, WireNode};
        let wire = WireNode {
            id: "w".to_string(),
            kind: NodeKind::Function,
            name: "Run".to_string(),
            qualified_name: "Run".to_string(),
            file_path: "server/run.go".to_string(),
            range: Range { start: Position { line: 0, col: 0 }, end: Position { line: 1, col: 0 } },
            signature: None,
            visibility: Visibility::Public,
            doc_comment: None,
            language: "go".to_string(),
            native_kind: None,
            has_syntax_errors: false,
            declarations: None,
            container: Some("github.com/x/app/server".to_string()),
            container_parent: Some("github.com/x/app".to_string()),
            target: None,
        };
        let mut conn = setup(false);
        apply_diff(&mut conn, &upsert(vec![crate::watcher::apply::to_node_record(wire)])).unwrap();
        assert_eq!(
            row(&conn, "go", "github.com/x/app/server").unwrap().parent_key.as_deref(),
            Some("github.com/x/app")
        );
    }

    // ---------------------------------------------------------------------
    // Sequence test: random diffs against a model, invariants after each.
    // ---------------------------------------------------------------------

    /// splitmix64: deterministic, dependency-free, and good enough to spread
    /// choices over a few dozen options. `proptest` is not a dependency of
    /// this crate, and a seed printed on failure plus
    /// [`SEED_ENV`] to replay it is all the shrinking this needs.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
        fn percent(&mut self, p: u64) -> bool {
            self.next() % 100 < p
        }
    }

    /// Replays one seed instead of the built-in set: `G_MESH_CONTAINERS_SEED=
    /// 0x1234 cargo test ... containers::tests::`.
    const SEED_ENV: &str = "G_MESH_CONTAINERS_SEED";

    fn seeds(default: &[u64]) -> Vec<u64> {
        match std::env::var(SEED_ENV) {
            Ok(value) => {
                let value = value.trim();
                let parsed = match value.strip_prefix("0x") {
                    Some(hex) => u64::from_str_radix(hex, 16),
                    None => value.parse(),
                };
                vec![parsed.unwrap_or_else(|_| panic!("{SEED_ENV}={value} is not a u64"))]
            }
            Err(_) => default.to_vec(),
        }
    }

    const LANGUAGES: [&str; 2] = ["go", "rust"];
    /// Keys and their true parents. A space in one key, a three-deep chain,
    /// and two roots.
    const KEYS: [(&str, Option<&str>); 5] = [
        ("app", None),
        ("app/srv", Some("app")),
        ("app/srv/http", Some("app/srv")),
        ("lib", None),
        ("lib util", Some("lib")),
    ];
    const FILES: usize = 6;
    const SLOTS: usize = 5;

    #[derive(Debug, Clone, PartialEq)]
    struct Spec {
        language: String,
        container: Option<String>,
        parent: Option<String>,
        native_kind: Option<String>,
        signature: u64,
    }

    impl Spec {
        fn random(rng: &mut Rng, file: usize) -> Self {
            let (key, parent) = KEYS[rng.below(KEYS.len())];
            Spec {
                // Mostly the file's own language; sometimes the other one, so
                // an id changing language is exercised too.
                language: LANGUAGES[if rng.percent(90) { file % 2 } else { (file + 1) % 2 }].to_string(),
                container: if rng.percent(85) { Some(key.to_string()) } else { None },
                parent: if rng.percent(92) { parent.map(str::to_string) } else { Some("bogus".to_string()) },
                native_kind: if rng.percent(10) {
                    Some(PENDING_SYMBOL_NATIVE_KIND.to_string())
                } else {
                    None
                },
                signature: rng.next() % 3,
            }
        }

        fn membership(&self) -> Option<Key> {
            membership(&self.language, self.container.as_deref(), self.native_kind.as_deref())
        }
    }

    fn node_id(file: usize, slot: usize) -> String {
        format!("f{file}s{slot}")
    }
    fn file_node_id(file: usize) -> String {
        format!("file{file}")
    }
    fn plugin_edge_id(file: usize, slot: usize) -> String {
        format!("pe{file}s{slot}")
    }

    fn record(file: usize, slot: usize, spec: &Spec) -> NodeRecord {
        let id = node_id(file, slot);
        let kind = if spec.native_kind.is_some() { MODULE_KIND } else { "Function" };
        let mut node = NodeRecord::new(&id, kind, &id, &id, format!("src/f{file}.x"), &spec.language);
        node.container = spec.container.clone();
        node.container_parent = spec.parent.clone();
        node.native_kind = spec.native_kind.clone();
        node.signature = Some(format!("sig {}", spec.signature));
        node
    }

    fn file_record(file: usize) -> NodeRecord {
        NodeRecord::new(file_node_id(file), "File", format!("f{file}.x"), "", format!("src/f{file}.x"), "go")
    }

    fn plugin_edge(file: usize, slot: usize) -> EdgeRecord {
        EdgeRecord::new(
            plugin_edge_id(file, slot),
            file_node_id(file),
            node_id(file, slot),
            "DEFINES",
            "tree-sitter",
            true,
        )
    }

    /// What a plugin would believe about the project, and what the index
    /// should therefore hold.
    #[derive(Default)]
    struct World {
        files: Vec<Option<BTreeMap<usize, Spec>>>,
        /// `(language, key) -> parent`, last writer wins, dropped at zero.
        parents: HashMap<Key, Option<String>>,
    }

    impl World {
        fn new() -> Self {
            World { files: vec![None; FILES], parents: HashMap::new() }
        }

        /// Records the parent claims of `diff`'s member records, in order,
        /// then forgets the parents of containers left with no members.
        fn note_parents(&mut self, diff: &Diff) {
            for node in &diff.upsert_nodes {
                if let Some(key) = record_membership(node) {
                    self.parents.insert(key, node.container_parent.clone());
                }
            }
            let live = self.expected_members();
            self.parents.retain(|key, _| live.contains_key(key));
        }

        fn expected_members(&self) -> BTreeMap<Key, Vec<String>> {
            let mut members: BTreeMap<Key, Vec<String>> = BTreeMap::new();
            for (file, slots) in self.files.iter().enumerate() {
                for (slot, spec) in slots.iter().flatten() {
                    if let Some(key) = spec.membership() {
                        members.entry(key).or_default().push(node_id(file, *slot));
                    }
                }
            }
            for ids in members.values_mut() {
                ids.sort();
            }
            members
        }

        fn check(&self, conn: &Connection) -> Result<(), String> {
            check_invariants(conn)?;
            let expected = self.expected_members();
            let actual: BTreeMap<Key, Vec<String>> = rows(conn)
                .into_iter()
                .map(|row| {
                    let members = edge_members(conn, &row.language, &row.key);
                    ((row.language, row.key), members)
                })
                .collect();
            if actual != expected {
                return Err(format!(
                    "membership differs from the model:\n  index: {actual:?}\n  model: {expected:?}"
                ));
            }
            for row in rows(conn) {
                let key = (row.language.clone(), row.key.clone());
                let expected_parent = self.parents.get(&key).cloned().flatten();
                if row.parent_key != expected_parent {
                    return Err(format!("{row:?}: parentKey should be {expected_parent:?}"));
                }
            }
            let plugin_edges =
                count(conn, "SELECT COUNT(*) FROM edges WHERE engine = 'tree-sitter'") as usize;
            let expected_edges: usize = self.files.iter().flatten().map(BTreeMap::len).sum();
            if plugin_edges != expected_edges {
                return Err(format!(
                    "{plugin_edges} plugin edges in the index, {expected_edges} in the model"
                ));
            }
            Ok(())
        }

        /// A plugin's reparse of `file`: slots appear, disappear, change
        /// container, kind or language, and only what changed is sent - plus
        /// a random share of unchanged nodes re-sent anyway, which a real
        /// plugin does whenever something upstream of the symbol moved.
        fn edit(&mut self, rng: &mut Rng, file: usize) -> Diff {
            let old = self.files[file].clone().unwrap_or_default();
            let mut new = old.clone();
            for slot in 0..SLOTS {
                match new.get(&slot).cloned() {
                    None => {
                        if rng.percent(45) {
                            new.insert(slot, Spec::random(rng, file));
                        }
                    }
                    Some(mut spec) => match rng.below(100) {
                        0..=17 => {
                            new.remove(&slot);
                        }
                        18..=47 => {
                            let fresh = Spec::random(rng, file);
                            spec.container = fresh.container;
                            spec.parent = fresh.parent;
                            new.insert(slot, spec);
                        }
                        48..=52 => {
                            spec.native_kind = match spec.native_kind {
                                Some(_) => None,
                                None => Some(PENDING_SYMBOL_NATIVE_KIND.to_string()),
                            };
                            new.insert(slot, spec);
                        }
                        53..=55 => {
                            spec.language = if spec.language == "go" { "rust" } else { "go" }.to_string();
                            new.insert(slot, spec);
                        }
                        56..=65 => {
                            spec.signature += 1;
                            new.insert(slot, spec);
                        }
                        _ => {}
                    },
                }
            }

            let mut diff = Diff::default();
            diff.upsert_nodes.push(file_record(file));
            for (slot, spec) in &new {
                let before = old.get(slot);
                if before != Some(spec) || rng.percent(25) {
                    diff.upsert_nodes.push(record(file, *slot, spec));
                }
                if before.is_none() || rng.percent(10) {
                    diff.upsert_edges.push(plugin_edge(file, *slot));
                }
            }
            for slot in old.keys().filter(|slot| !new.contains_key(slot)) {
                diff.delete_node_ids.push(node_id(file, *slot));
                diff.delete_edge_ids.push(plugin_edge_id(file, *slot));
            }
            self.files[file] = Some(new);
            diff
        }

        fn delete_file(&mut self, file: usize) -> Diff {
            let mut diff = Diff::default();
            if let Some(slots) = self.files[file].take() {
                for slot in slots.keys() {
                    diff.delete_node_ids.push(node_id(file, *slot));
                    diff.delete_edge_ids.push(plugin_edge_id(file, *slot));
                }
                diff.delete_node_ids.push(file_node_id(file));
            }
            diff
        }

        /// Rotates the containers of `file`'s nodes one step, so every member
        /// moves at once and several containers each lose one member and gain
        /// another inside a single diff.
        fn rotate(&mut self, file: usize) -> Diff {
            let mut diff = Diff::default();
            let Some(slots) = self.files[file].as_mut() else { return diff };
            let containers: Vec<(Option<String>, Option<String>)> =
                slots.values().map(|spec| (spec.container.clone(), spec.parent.clone())).collect();
            if containers.len() < 2 {
                return diff;
            }
            for (i, (slot, spec)) in slots.iter_mut().enumerate() {
                let (container, parent) = containers[(i + 1) % containers.len()].clone();
                spec.container = container;
                spec.parent = parent;
                diff.upsert_nodes.push(record(file, *slot, spec));
            }
            diff
        }

        /// One id sent several times in one diff - first to a random container,
        /// then to its final one - plus a delete-and-re-add of another id:
        /// both shapes a burst of merged diffs produces.
        fn repeat_records(&mut self, rng: &mut Rng, file: usize) -> Diff {
            let mut diff = Diff::default();
            let Some(slots) = self.files[file].as_mut() else { return diff };
            let present: Vec<usize> = slots.keys().copied().collect();
            if present.is_empty() {
                return diff;
            }
            let slot = present[rng.below(present.len())];
            let interim = Spec::random(rng, file);
            diff.upsert_nodes.push(record(file, slot, &interim));
            let fin = Spec::random(rng, file);
            diff.upsert_nodes.push(record(file, slot, &fin));
            slots.insert(slot, fin);

            let other = present[rng.below(present.len())];
            if other != slot {
                let spec = slots[&other].clone();
                diff.delete_edge_ids.push(plugin_edge_id(file, other));
                diff.delete_node_ids.push(node_id(file, other));
                diff.upsert_nodes.push(record(file, other, &spec));
                diff.upsert_edges.push(plugin_edge(file, other));
            }
            diff
        }
    }

    fn merge(mut into: Diff, other: Diff) -> Diff {
        into.upsert_nodes.extend(other.upsert_nodes);
        into.delete_node_ids.extend(other.delete_node_ids);
        into.upsert_edges.extend(other.upsert_edges);
        into.delete_edge_ids.extend(other.delete_edge_ids);
        into
    }

    /// 8 seeds x 250 diffs x both foreign-key settings = 4,000 diffs, each
    /// followed by a full invariant check. Sized by what it exercises rather
    /// than by a round number: the run prints how many containers it created
    /// and garbage-collected, and asserts both are well into the hundreds.
    const SEQUENCE_SEEDS: [u64; 8] = [1, 2, 3, 0xC0FFEE, 0xDEAD_BEEF, 42, 1729, 0x5EED_0265];
    const SEQUENCE_STEPS: usize = 250;

    /// The acceptance criterion: over hundreds of random upsert/delete/move
    /// diffs across several files, containers and two languages, after every
    /// single diff, `memberCount` equals the container's `DEFINES` edges,
    /// no empty container exists, and membership and parents equal what a
    /// model of the plugin's own view says they should be - on connections
    /// with foreign keys both on and off.
    #[test]
    fn membership_invariants_hold_after_every_diff_of_a_random_sequence() {
        let (mut diffs, mut materialized, mut collected) = (0usize, 0usize, 0usize);
        for seed in seeds(&SEQUENCE_SEEDS) {
            for foreign_keys in [true, false] {
                let mut conn = setup(foreign_keys);
                let mut rng = Rng(seed);
                let mut world = World::new();
                for step in 0..SEQUENCE_STEPS {
                    let file = rng.below(FILES);
                    let (op, diff) = match rng.below(100) {
                        0..=49 => ("edit", world.edit(&mut rng, file)),
                        50..=59 => ("delete file", world.delete_file(file)),
                        60..=74 => {
                            let second = (file + 1 + rng.below(FILES - 1)) % FILES;
                            let first = world.edit(&mut rng, file);
                            ("burst of two files", merge(first, world.edit(&mut rng, second)))
                        }
                        75..=87 => ("rotate containers", world.rotate(file)),
                        _ => ("repeated records", world.repeat_records(&mut rng, file)),
                    };
                    world.note_parents(&diff);
                    let context = format!(
                        "seed {seed:#x}, step {step} ({op} on f{file}), foreign_keys={foreign_keys} - replay \
                         with {SEED_ENV}={seed:#x}"
                    );
                    let before: BTreeSet<String> = rows(&conn).into_iter().map(|row| row.node_id).collect();
                    if let Err(err) = apply_diff(&mut conn, &diff) {
                        panic!("{context}: apply_diff failed: {err:#}");
                    }
                    if let Err(message) = world.check(&conn) {
                        panic!("{context}: {message}");
                    }
                    let after: BTreeSet<String> = rows(&conn).into_iter().map(|row| row.node_id).collect();
                    materialized += after.difference(&before).count();
                    collected += before.difference(&after).count();
                    diffs += 1;
                }
            }
        }
        // A sequence that never creates or empties a container proves
        // nothing about either; make sure this one does both, a lot.
        eprintln!(
            "containers sequence test: {diffs} diffs, {materialized} containers materialized, {collected} collected"
        );
        assert!(materialized > 100 && collected > 100, "{materialized} materialized, {collected} collected");
    }

    // ---------------------------------------------------------------------
    // Batch boundaries.
    // ---------------------------------------------------------------------

    enum Item {
        Node(usize, usize, Spec),
        File(usize),
        Edge(usize, usize),
    }

    /// A bulk stream in a plugin's own order - each file's `File` node, its
    /// declarations, then its edges - followed by a second pass that re-sends
    /// some declarations with a different container or parent, so a cut can
    /// fall between an id's two records as well as between a container's
    /// members.
    fn bulk_stream(rng: &mut Rng) -> Vec<Item> {
        let mut items = Vec::new();
        let mut sent = Vec::new();
        for file in 0..FILES {
            items.push(Item::File(file));
            let slots: Vec<usize> = (0..SLOTS).filter(|_| rng.percent(80)).collect();
            for slot in &slots {
                let spec = Spec::random(rng, file);
                sent.push((file, *slot, spec.clone()));
                items.push(Item::Node(file, *slot, spec));
            }
            for slot in slots {
                items.push(Item::Edge(file, slot));
            }
        }
        for (file, slot, mut spec) in sent {
            if rng.percent(30) {
                let fresh = Spec::random(rng, file);
                spec.container = fresh.container;
                spec.parent = fresh.parent;
                items.push(Item::Node(file, slot, spec));
            }
        }
        items
    }

    fn batch(items: &[Item]) -> Diff {
        let mut diff = Diff::default();
        for item in items {
            match item {
                Item::Node(file, slot, spec) => diff.upsert_nodes.push(record(*file, *slot, spec)),
                Item::File(file) => diff.upsert_nodes.push(file_record(*file)),
                Item::Edge(file, slot) => diff.upsert_edges.push(plugin_edge(*file, *slot)),
            }
        }
        diff
    }

    /// Everything the index holds, as sorted text: the whole `containers`
    /// table, every node and every edge.
    fn snapshot(conn: &Connection) -> Vec<String> {
        let mut out = Vec::new();
        for sql in [
            "SELECT language || '|' || key || '|' || nodeId || '|' || IFNULL(parentKey, '<null>') || '|' || memberCount FROM containers ORDER BY 1",
            "SELECT id || '|' || kind || '|' || name || '|' || filePath || '|' || language || '|' || IFNULL(nativeKind, '') || '|' || IFNULL(container, '') || '|' || visibility FROM nodes ORDER BY 1",
            "SELECT id || '|' || fromId || '|' || toId || '|' || kind || '|' || source || '|' || engine || '|' || resolved FROM edges ORDER BY 1",
        ] {
            let mut stmt = conn.prepare_cached(sql).unwrap();
            let rows: Vec<String> =
                stmt.query_map([], |row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
            out.extend(rows);
            out.push("--".to_string());
        }
        out
    }

    fn apply_cut(stream: &[Item], cuts: &[usize], foreign_keys: bool) -> Vec<String> {
        let mut conn = setup(foreign_keys);
        let mut start = 0;
        for &end in cuts.iter().chain(std::iter::once(&stream.len())) {
            apply_diff(&mut conn, &batch(&stream[start..end])).unwrap();
            start = end;
        }
        check_invariants(&conn).unwrap();
        snapshot(&conn)
    }

    /// `daemon::bulk_index::commit` is one `apply_diff` per batch, and the
    /// batch can end anywhere. The same stream committed whole, one item at a
    /// time, in fixed-size chunks and at random cut points must leave
    /// byte-identical containers, nodes and edges.
    #[test]
    fn bulk_batch_boundaries_do_not_change_the_result() {
        for seed in seeds(&[11, 12, 13, 0xB01C]) {
            let mut rng = Rng(seed);
            let stream = bulk_stream(&mut rng);
            let whole = apply_cut(&stream, &[], true);

            // The comparison has to be able to fail: the same stream minus
            // its re-sent tail is a different index.
            let first_pass =
                stream.iter().position(|item| matches!(item, Item::File(f) if *f == FILES - 1)).unwrap();
            let tail_start = stream
                .iter()
                .enumerate()
                .skip(first_pass)
                .find(|(_, item)| matches!(item, Item::Node(f, _, _) if *f < FILES - 1))
                .map(|(i, _)| i);
            if let Some(tail_start) = tail_start {
                assert_ne!(
                    apply_cut(&stream[..tail_start], &[], true),
                    whole,
                    "seed {seed:#x}: the snapshot cannot tell two different streams apart"
                );
            }
            assert!(
                whole.iter().any(|line| line.contains("|container|")),
                "seed {seed:#x}: no container at all"
            );

            let mut cut_sets: Vec<Vec<usize>> = Vec::new();
            for size in [1, 2, 3, 5, 8, 13] {
                cut_sets.push((size..stream.len()).step_by(size).collect());
            }
            for _ in 0..25 {
                let mut cuts: Vec<usize> =
                    (0..1 + rng.below(6)).map(|_| 1 + rng.below(stream.len() - 1)).collect();
                cuts.sort_unstable();
                cuts.dedup();
                cut_sets.push(cuts);
            }
            for cuts in cut_sets {
                for foreign_keys in [true, false] {
                    assert_eq!(
                        apply_cut(&stream, &cuts, foreign_keys),
                        whole,
                        "seed {seed:#x}, cuts {cuts:?}, foreign_keys={foreign_keys}: replay with {SEED_ENV}={seed:#x}"
                    );
                }
            }
        }
    }
}
