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
//!  1. `storage::containers::detach` runs first, before `apply_diff` deletes or upserts anything.
//!     For every node the diff names, it reads the membership the index holds
//!     for it *now* and compares it with the membership the diff leaves it with
//!     (the last upsert of that id, or none if the diff only deletes it). Where
//!     they differ, the old `DEFINES` edge is deleted. This must happen before
//!     the node delete: `edges.toId` is a foreign key, and in the test
//!     connections that enforce it a member cannot be deleted while an edge
//!     still points at it.
//!  2. `storage::containers::attach` runs last, once every node is written. Each member the diff
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
//! `link::imports::link_diff`/`link::symbol_links::link_diff` run after the
//! commit, and can afford to: an unlinked placeholder is a *correct* graph with
//! less in it, and the next pass finishes the job. Half-applied membership is
//! not correct - a committed member delete whose container still counts it, or
//! a new member with no container node, is exactly the drift this module exists
//! to rule out - so membership commits or rolls back with the diff that caused
//! it. It also covers every writer at once: the per-file reparse
//! (`ingest::apply`), each cold-start bulk batch (`daemon::bulk_index::commit`),
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
//! things break if a placeholder is counted: `link::imports` deletes a
//! linked-away `resolved_module` placeholder with plain SQL (outside
//! `apply_diff`, so this module would never see it leave), and it only does
//! so once no edge touches the placeholder - which a `DEFINES` edge from its
//! container would prevent forever. An empty key is treated as no container
//! rather than as a container whose key is the empty string, which no
//! language in the design doc's table has.
//!
//! `storage::containers::membership` reads this exclusion off `graph::queries::
//! NON_DECLARATION_NATIVE_KINDS` (GM-376) - the same list `find_by_name`,
//! `find_by_qualified_name`, `find_by_position` and `mcp::find_definition`'s
//! candidate page consult, and the one `link::symbol_links::is_declaration`
//! has read since GM-372 - rather than spelling its own copy. It had been a
//! four-kind copy missing `external_module` until now, which
//! `link::symbol_links::requesters_below_new_containers`' own comment names
//! as the coupling to watch: that function compares its upserted-member count
//! against the `containers.memberCount` this module maintains, so the two
//! must exclude the same kinds. They agreed on everything any plugin actually
//! sends only because an import record carries no container at all, so it is
//! filtered out one line above by the `is_empty` check regardless of which
//! list is consulted - see `storage::containers::tests::
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
//! non-eager behaviour `link::imports` documents for a deleted target file
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

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::model::kinds::{declaration_only, DEFINES_KIND, FILE_KIND, MODULE_KIND};

pub use crate::model::container_id;
pub use g_mesh_wire::native_kind::CONTAINER_NATIVE_KIND;

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

/// Container node id -> container key.
pub fn container_keys(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut stmt = conn
        .prepare("SELECT nodeId, key FROM containers")
        .context("failed to prepare the container listing")?;
    let rows =
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?))).context("failed to list the containers")?;
    rows.collect::<rusqlite::Result<_>>().context("failed to read the containers")
}

/// [`defining_containers`] for every `File` node at once, inverted: each
/// container's node id to the paths of the files that define it, sorted.
/// A container no file defines has no entry (see [`member_files`]).
pub fn defining_files(conn: &Connection) -> Result<HashMap<String, Vec<String>>> {
    let mut stmt = conn
        .prepare("SELECT filePath FROM nodes WHERE kind = ?1 ORDER BY filePath")
        .context("failed to prepare the file listing")?;
    let files: Vec<String> = stmt
        .query_map(params![FILE_KIND], |row| row.get(0))
        .context("failed to list the indexed files")?
        .collect::<rusqlite::Result<_>>()
        .context("failed to read the indexed files")?;
    let mut by_container: HashMap<String, Vec<String>> = HashMap::new();
    for file in files {
        for container in defining_containers(conn, &file)? {
            by_container.entry(container.node_id).or_default().push(file.clone());
        }
    }
    Ok(by_container)
}

/// A container's `DEFINES` members' files, split by what the member is.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MemberFiles {
    /// Files holding an ordinary member (a function, a variable, ...), sorted.
    pub declarations: Vec<String>,
    /// Files whose own whole-file `Module` node is a member: sub-modules, not
    /// the container itself (see [`defining_containers`]), sorted.
    pub submodules: Vec<String>,
}

/// The files holding a container's `DEFINES` members: the fallback for a
/// container [`defining_files`] maps to no file.
pub fn member_files(conn: &Connection, container_node_id: &str) -> Result<MemberFiles> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT DISTINCT m.filePath, \
                    (m.kind = ?3 AND m.startLine <= f.startLine AND m.endLine >= f.endLine) \
             FROM edges e JOIN nodes m ON m.id = e.toId \
             JOIN nodes f ON f.kind = ?4 AND f.filePath = m.filePath \
             WHERE e.fromId = ?1 AND e.kind = ?2 AND m.filePath <> '' ORDER BY m.filePath",
        )
        .context("failed to prepare the container member lookup")?;
    let rows = stmt
        .query_map(params![container_node_id, DEFINES_KIND, MODULE_KIND, FILE_KIND], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
        })
        .context("failed to look up a container's member files")?;
    let mut files = MemberFiles::default();
    for row in rows {
        let (path, whole_file_module) = row.context("failed to read a container's member files")?;
        let list = if whole_file_module { &mut files.submodules } else { &mut files.declarations };
        if list.last() != Some(&path) {
            list.push(path);
        }
    }
    Ok(files)
}

/// Files whose own whole-file `Module` declaration is named `key` in
/// `language`, sorted: the file that *is* a module that declares nothing
/// (a Python package's `__init__.py` holding only imports or nothing at all
/// below the package root).
pub fn own_module_files(conn: &Connection, language: &str, key: &str) -> Result<Vec<String>> {
    let sql = format!(
        "SELECT DISTINCT m.filePath FROM nodes m \
         JOIN nodes f ON f.kind = ?3 AND f.filePath = m.filePath \
         WHERE m.kind = ?4 AND m.language = ?1 AND m.qualifiedName = ?2 AND m.filePath <> '' \
           AND {} AND m.startLine <= f.startLine AND m.endLine >= f.endLine \
         ORDER BY m.filePath",
        declaration_only("m.")
    );
    let mut stmt = conn.prepare_cached(&sql).context("failed to prepare the module file lookup")?;
    let rows = stmt
        .query_map(params![language, key, FILE_KIND, MODULE_KIND], |row| row.get(0))
        .context("failed to look up a module's own file")?;
    rows.collect::<rusqlite::Result<_>>().context("failed to read a module's own file")
}

/// Whether the index holds a `File` node at `file_path`.
pub fn has_file(conn: &Connection, file_path: &str) -> Result<bool> {
    conn.prepare_cached("SELECT EXISTS (SELECT 1 FROM nodes WHERE kind = ?1 AND filePath = ?2)")
        .context("failed to prepare the file lookup")?
        .query_row(params![FILE_KIND, file_path], |row| row.get(0))
        .context("failed to look up a file")
}
