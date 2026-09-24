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
mod tests;
