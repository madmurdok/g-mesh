//! The per-language reindex a settled edit to one of a manifest's
//! `[plugin.workspace] watch_files` triggers (`go.mod`, `Cargo.toml`,
//! `*.csproj`...) - `docs/architecture/multi-language-plugins.md`'s "Editing
//! a Go file" data-flow paragraph, verbatim: "`watch_files` ->
//! `workspaceChanged` -> per-language reindex: delete that language's rows,
//! bulk, link, semantic." `daemon::registry::PluginRegistry::
//! workspace_language_matches` decides *whether* a settled path triggers
//! this; this module is what actually runs it, once it has.
//!
//! # Why a workspace file needs a *reindex* and not a reparse
//!
//! An ordinary `fileChanged` diffs one file against its own previous
//! extraction - correct, because nothing about *other* files changed. A
//! workspace file is different in kind: `go.mod`'s `module` line, or
//! `Cargo.toml`'s `[lib] path`/workspace `members`, decides *where module and
//! crate boundaries are*, which every other file's container key
//! (`graph::containers`) and every placeholder's scope (`graph::symbol_links`,
//! `graph::imports`) were computed against. A single-file diff cannot correct
//! that - the plugin would have to re-derive every other file's container
//! from scratch to answer honestly, which is exactly what a full walk of the
//! language already does. So instead of asking the plugin to diff one file,
//! core throws away everything it currently believes about this language and
//! asks for it again from nothing - the same one-shot `--bulk-index` machinery
//! the cold start already uses, restricted to one manifest.
//!
//! # Decision 1: what "that language's rows" means
//!
//! Deleted, precisely, by [`delete_language_rows`]:
//!
//!  - **Nodes**: every row of `nodes WHERE language = ?` - member
//!    declarations and the container nodes `graph::containers` materialized
//!    for this language alike (a container node's own `language` column is
//!    always the language of its members - `graph::containers::ensure_container`
//!    - so one `WHERE language = ?` sweeps both without a separate
//!      `nativeKind = 'container'` branch).
//!  - **Edges**: every edge with *either* endpoint among those nodes.
//!    Cross-language edges do not exist today (the architecture doc's
//!    Non-goals), so in practice this is every edge wholly inside the
//!    language - but the query does not assume that, matching this task's
//!    "still don't leave dangling ones" instruction: an edge is matched by
//!    where its endpoints actually are, not by an assumption about what kind
//!    of project this is.
//!  - **`containers` rows**: every row `WHERE language = ?` - the
//!    GM-265-handoff requirement, satisfied structurally rather than by a
//!    special case: containers are unique per `(language, key)`, so deleting
//!    *every* node of a language necessarily empties *every* container of
//!    that language at once. There is no "some members deleted, container
//!    survives" case to reason about here the way `graph::containers::attach`'s
//!    incremental recount has to for an ordinary diff.
//!  - **`declarations`/`vectors`/`placeholder_targets`**: every child row
//!    keyed by a deleted node id. `storage::write::apply_diff`'s own
//!    `delete_node_ids` loop explicitly deletes `declarations`,
//!    `placeholder_targets` and `vectors` this same way (all three tables
//!    carry `ON DELETE CASCADE`, but `storage::connection::open` switches
//!    `foreign_keys` off on the daemon's real connection - see that
//!    function's own doc). When this module was written `apply_diff` did not
//!    yet delete `vectors`, a gap this delete closed for itself; GM-294 closed
//!    it there too, once GM-292 showed the connection had in fact been
//!    enforcing foreign keys all along, so the cascade had been hiding it.
//!    Leaving a deleted node's embedding behind would hand it to whatever
//!    content-derived id a rebuilt node happens to collide with, the exact
//!    hazard `apply_diff`'s own comment warns about.
//!  - **`indexed_files`**: deliberately **not** touched. That table is a
//!    disk-truth baseline (`watcher::staleness`: "what's recorded... and, on
//!    a mismatch, reindexes") - it says whether a file's *bytes on disk*
//!    still match what core last saw, which has nothing to do with whether
//!    core's *derived graph* for that file is about to be rebuilt. A
//!    workspace-file edit does not touch any other file's bytes, so every
//!    other file's `indexed_files` baseline is still true after this reindex
//!    exactly as it was before it - deleting or re-stamping those rows would
//!    cost a full-table scan (there is no index on `filePath` by extension)
//!    for no correctness gain, and would actively be wrong: the next
//!    unrelated edit to one of this language's files would see a missing
//!    baseline and treat a file that never changed as needing a reparse it
//!    does not need. (The changed workspace file itself - `go.mod` - is not
//!    a plugin-indexed source file at all, so it was never one of
//!    `indexed_files`' rows to begin with, extension routing being how a
//!    file ever gets one in the first place.)
//!  - **`language_state`**: the whole row is deleted (`bulkIndexedAt` and
//!    `semanticPassAt` both go with it), then re-recorded from scratch as the
//!    reindex actually completes each phase - `bulkIndexedAt` by
//!    `daemon::bulk_index::walk_one_language` itself (unchanged; it already
//!    calls `schema::record_language_bulk_indexed` once its walk lands),
//!    `semanticPassAt` by [`run`] below, once the (possibly absent) semantic
//!    phase actually finishes. A language whose reindex fails partway
//!    through is left with no `language_state` row at all rather than a
//!    stale one claiming a pass that no longer describes the rebuilt graph -
//!    the same honesty `storage::schema`'s own module doc insists on
//!    project-wide, applied per language.
//!  - **Meta roll-ups**: [`run`] re-checks both `meta.bulkIndexedAt`
//!    (`schema::record_bulk_index`) and `meta.semanticPassAt`
//!    (`schema::reconcile_semantic_pass_rollup`) after its own phases land,
//!    the same two calls `daemon::semantic::run_with_registry` already makes
//!    per whole-project pass, run here per reindex instead.
//!
//! # Decision: targeted SQL, not a `storage::write::Diff` of every deleted id
//!
//! GM-265's handoff on this task raised the alternative directly: route the
//! delete through `apply_diff` as a `Diff` of `delete_node_ids`, so
//! `graph::containers::detach`/`attach` run and maintain the invariant by the
//! same path an ordinary edit does. [`delete_language_rows`] does not do
//! that - it is seven `DELETE ... WHERE language = ?` (or `WHERE nodeId IN
//! (SELECT id FROM nodes WHERE language = ?)`) statements in one transaction,
//! and the reasoning is cost, exactly as GM-265 asked to see spelled out:
//!
//!  - **What the `Diff` route would cost.** `apply_diff`'s `delete_node_ids`
//!    loop issues three `.execute()` calls per id (`declarations`,
//!    `placeholder_targets`, `nodes`), and `containers::detach` issues one
//!    more (`SELECT language, container, nativeKind FROM nodes WHERE id =
//!    ?1`) per id *before* any of those - four individual prepared-statement
//!    round trips per node, all through the Rust/SQLite FFI boundary, for
//!    every one of a large language's nodes. At 100,000 nodes that is
//!    400,000+ separate `execute`/`query_row` calls, each paying FFI
//!    marshaling and (`prepare_cached` aside) statement-step overhead on top
//!    of whatever work SQLite itself does - and `containers::attach`'s own
//!    module doc already measures the *cheaper* half of this shape (one
//!    `COUNT(*)` recount per touched container) at ~16ms for a 100,000-member
//!    container, which is the cost of *one* of those four per-id operations,
//!    run as a single set-based query instead of 100,000 individual ones.
//!  - **What the targeted-SQL route costs instead.** A fixed **seven**
//!    statements, each a single `DELETE` that SQLite's own query planner
//!    executes as one pass over an index (`idx_edges_fromId`/
//!    `idx_edges_toId` for the edge delete, `idx_nodes_container`'s
//!    `(language, container)` prefix - or a full scan of `nodes`, bounded by
//!    the language's own row count either way - for the rest), independent
//!    of how many rows they touch. No Rust-side loop, no per-row FFI call,
//!    no 999-bound-parameter ceiling to work around (`nodes.id` is never
//!    passed as an `IN (?, ?, ...)` list of individual ids - every predicate
//!    here is either `language = ?1` directly or a subquery on it).
//!  - **Why the invariant still holds without the incremental machinery.**
//!    `containers::detach`/`attach` exist to get an *ordinary* diff's
//!    membership changes right when only *some* of a container's members
//!    move - the hard case is "did this specific node's container change,
//!    and does its old container now have zero members". A language-wide
//!    delete never asks that question: **every** member of **every**
//!    container of this language is leaving at once, so every one of that
//!    language's containers becomes empty by construction, not by recount -
//!    deleting `containers WHERE language = ?` *is* the correct answer, not
//!    an approximation of one. The bulk walk that follows goes back through
//!    the ordinary `apply_diff` -> `containers::detach`/`attach` path for
//!    every batch it commits (`daemon::bulk_index::commit` is unchanged),
//!    so every container this reindex ends with was built by the same
//!    incremental machinery, with the same guarantees, as any other bulk
//!    index - this module only has to get the *deletion* right, not
//!    reimplement attachment too.
//!
//! `delete_language_rows_test_invariants_hold_after_reindex` (this module's
//! own tests) is what actually checks the GM-265 handoff's acceptance
//! criterion - `memberCount == DEFINES edges`, no zero-member container rows
//! surviving - against a real post-reindex index, not just against this
//! reasoning.
//!
//! # Decision 2: concurrency with the ordinary per-file stream
//!
//! [`run`] does its delete/walk/link phase inside
//! `PluginSupervisor::with_exclusive_access` - the *same* lock
//! `PluginSupervisor::file_changed`/`replay_pending`/`ensure_fresh`/
//! `semantic_pass` already take for the duration of their own round trip to
//! this language's plugin (see that method's own doc comment). No new lock is
//! invented: an ordinary settled edit to one of this language's files that
//! arrives mid-reindex blocks on the exact mutex it was always going to
//! contend on, and only resumes once the delete/walk/link phase has
//! committed, at which point it can neither commit a diff between the delete
//! and the walk (which the walk's own re-populate would then silently
//! overwrite or leave orphaned) nor slip in after the walk but describe a
//! graph the delete is about to erase. The semantic phase runs *after* that
//! lock is released,
//! through the ordinary `PluginSupervisor::semantic_pass` (which takes the
//! same lock itself, for its own single round trip) - safe to leave
//! unserialized with the delete/walk/link phase specifically because, by the
//! time it runs, this language's structural graph is already back to a
//! normal, fully-linked, self-consistent state; a semantic pass overlapping
//! an *ordinary* reparse of some other file in this language is exactly the
//! everyday case `daemon::semantic::run_with_registry` already accepts.
//!
//! # Decision 3: `workspaceChanged` and plugins that do not know it
//!
//! [`run`] sends the notification (`PluginProcess::notify_workspace_changed`)
//! only when this language's supervisor is currently *awake* (nothing here
//! wakes a sleeping one just to tell it something it has no cache to drop
//! for), and only ever for a language `daemon::registry::PluginRegistry::
//! workspace_language_matches` already required to have a non-empty
//! `watch_files` - which by construction excludes the bundled TS plugin
//! (`watch_files = []`) from ever reaching this module at all. Checked
//! against `plugins/typescript/src/index.ts`'s `handleEnvelope`: its
//! `switch (envelope.method)` has no `default` arm, so an unrecognized
//! method (this notification, if it were ever sent to a plugin build that
//! predates it) falls through doing nothing, and - because a notification
//! carries no `id` - the trailing `if (envelope.id !== undefined)` also does
//! nothing, so the frame is silently and safely ignored either way. That
//! makes the "don't send it" rule belt-and-braces rather than load-bearing on
//! its own for the one plugin in this repo, but it is still the documented
//! contract for a third-party manifest whose plugin might not tolerate an
//! unrecognized *request* the same way TS's notification-shaped no-op does.
//!
//! # Decision 4: debounce
//!
//! Not built here, because nothing needs to be: [`daemon::registry`]'s
//! caller is `daemon::watch_and_route_once`, which already runs every settled
//! path through `watcher::debounce::Debouncer` before it ever reaches
//! `route_settled_path` - the exact same trailing-edge coalescing an ordinary
//! source file's rapid re-saves already get (`daemon::DEBOUNCE_WINDOW`, task
//! 129). A burst of saves to `go.mod` inside one debounce window collapses to
//! one settled event for `"go.mod"`, which is one call into this module - no
//! second coalescing layer is needed on top, because there is only ever one
//! *path* here to begin with (`go.mod` is one file, not many), unlike
//! `watcher::burst::BurstBatcher`'s job of coalescing *several different
//! files'* diffs, which this reindex has no equivalent of.
//!
//! # Decision 6: where the walk and the semantic pass actually run
//!
//! The bulk walk reuses `daemon::bulk_index::walk_one_language` unchanged -
//! the same one-shot `--bulk-index` process the cold start spawns per
//! language, called here for a single manifest instead of iterated over
//! every discovered one, so this module owns no second implementation of
//! "spawn a plugin's bulk mode and ingest its NDJSON stream" to keep in sync
//! with the first. Linking (`graph::imports::link_all`, `graph::symbol_links
//! ::link_all`) reuses the exact calls `daemon::bulk_index::run` already
//! makes, project-wide rather than scoped to one language - correct because
//! neither function does anything but re-attempt whatever placeholders are
//! currently unresolved, in whichever language they belong to, and a
//! placeholder this reindex did not touch simply finds nothing new to link
//! against. The semantic phase reuses `daemon::semantic`'s per-language
//! primitives (`PluginSupervisor::semantic_pass`,
//! `daemon::semantic::indexed_file_count`, `storage::schema::
//! record_language_semantic_pass`/`reconcile_semantic_pass_rollup`) rather
//! than `daemon::semantic::run_with_registry` itself, because that entry
//! point asks *every currently-owed* language - which could include some
//! other language whose pass was separately interrupted - where this reindex
//! must ask about `language` alone, restricted exactly the way this task
//! requires.

use std::collections::HashSet;

use anyhow::{Context, Result};

use crate::daemon::bulk_index::{self, WalkContext};
use crate::daemon::lifecycle::PluginSupervisor;
use crate::daemon::registry::PluginRegistry;
use crate::daemon::semantic;
use crate::storage::index_store::IndexStore;
use crate::storage::schema;

/// Runs `language`'s whole per-language reindex against `registry`/
/// `supervisor`, in response to a settled edit of `changed_file` (one of that
/// language's own `[plugin.workspace] watch_files`) - see this module's doc
/// comment for the full sequence and the reasoning behind each phase.
/// `supervisor` must be `registry.get_or_spawn(language)`'s own supervisor
/// for `language` - callers reach this exclusively through
/// `PluginRegistry::workspace_file_changed`, which already guarantees that.
pub(crate) fn run(
    registry: &PluginRegistry,
    supervisor: &PluginSupervisor,
    store: &IndexStore,
    changed_file: &str,
) -> Result<()> {
    let manifest = supervisor.manifest().clone();

    // The locked phase: notify (best-effort), delete, re-walk, re-link, roll
    // up the bulk-index fact - see this module's doc comment ("Decision 2")
    // for why this whole sequence shares `PluginSupervisor`'s own
    // serialization lock instead of a second one of its own.
    supervisor.with_exclusive_access(|process| -> Result<()> {
        if let Some(process) = process {
            if let Err(err) = process.notify_workspace_changed(changed_file) {
                eprintln!(
                    "g-mesh daemon: failed to notify the {} plugin that {changed_file} changed \
                     ({err:#}) - it keeps whatever module/crate map it had cached, but the \
                     reindex below rebuilds the graph from scratch regardless",
                    manifest.language
                );
            }
        }

        store
            .delete_language(&manifest.language)
            .with_context(|| format!("failed to delete {}'s rows before reindexing it", manifest.language))?;

        // No `walked_files`, so no baselines from a one-language re-walk: see
        // this module's own doc comment on why it leaves `indexed_files` alone.
        let mut ctx =
            WalkContext { embedding: Some(registry.embedding().as_ref()), ..WalkContext::new(store) };
        bulk_index::walk_one_language(registry.project_root(), &manifest, &mut ctx).with_context(|| {
            format!("failed to re-walk {} after {changed_file} changed", manifest.language)
        })?;

        store.relink_after_language_reindex()?;
        Ok(())
    })?;

    // The unlocked phase: the semantic pass, restricted to this one language
    // - see this module's doc comment ("Decision 6") for why this calls
    // the per-language primitives directly rather than
    // `daemon::semantic::run_with_registry`, which would ask every currently-
    // owed language, not just this one.
    if manifest.capabilities.semantic_pass {
        let file_count = semantic::indexed_file_count(store, &manifest.language);
        match supervisor.semantic_pass(store, Vec::new(), file_count) {
            Ok(true) => {
                let recorded =
                    store.with(|conn| schema::record_language_semantic_pass(conn, &manifest.language));
                if let Err(err) = recorded {
                    eprintln!(
                        "g-mesh daemon: failed to record {}'s semantic pass after a workspace \
                         reindex ({err:#})",
                        manifest.language
                    );
                    semantic::record_failure(store, &manifest.language, &err);
                }
            }
            // The supervisor was asleep and deliberately left that way - see
            // `PluginSupervisor::semantic_pass`'s own doc comment. The
            // language stays owed for whoever next asks
            // (`daemon::semantic::run_with_registry`, on a future daemon
            // start, or a later reindex of this same language); status shows
            // why.
            Ok(false) => semantic::record_not_run(store, &manifest.language),
            Err(err) => {
                eprintln!(
                    "g-mesh daemon: the {} semantic pass after a workspace reindex failed ({err:#}) - \
                     its edges keep whatever the structural pass resolved",
                    manifest.language
                );
                semantic::record_failure(store, &manifest.language, &err);
            }
        }

        let capable: HashSet<String> = registry.semantic_pass_languages().into_iter().collect();
        if let Err(err) = store.with(|conn| schema::reconcile_semantic_pass_rollup(conn, &capable)) {
            eprintln!("g-mesh daemon: failed to update the project-wide semantic-pass roll-up ({err:#})");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use std::path::PathBuf;
    use std::sync::Arc;

    use rusqlite::params;

    use crate::storage::write::delete_language_rows;

    use super::*;
    use crate::daemon::manifest::discover;
    use crate::daemon::test_plugin;
    use crate::embedding::EmbeddingPipeline;
    use crate::storage::write::{apply_diff, Diff, NodeRecord};

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    fn node(id: &str, language: &str, file_path: &str, container: Option<&str>) -> NodeRecord {
        let mut node = NodeRecord::new(id, "Function", id, id, file_path, language);
        node.container = container.map(str::to_string);
        node
    }

    // -----------------------------------------------------------------
    // `delete_language_rows` on its own - no plugin, no registry. FK
    // enforcement ON (unlike the daemon's real connection), matching
    // `storage::write`'s own tests: it is what would actually catch a wrong
    // delete order here, where `storage::connection::open`'s FK-off
    // production connection would silently tolerate one.
    // -----------------------------------------------------------------

    #[test]
    fn delete_language_rows_removes_only_the_named_languages_nodes_edges_and_containers() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::apply(&conn).unwrap();

        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    node("a1", "alpha", "a.alpha", Some("pkg")),
                    node("a2", "alpha", "a.alpha", Some("pkg")),
                    node("b1", "beta", "b.beta", Some("mod")),
                ],
                upsert_edges: vec![crate::storage::write::EdgeRecord::new(
                    "e-a1-a2",
                    "a1",
                    "a2",
                    "CALLS",
                    "tree-sitter",
                    false,
                )],
                ..Default::default()
            },
        )
        .unwrap();
        schema::record_language_bulk_indexed(&conn, "alpha", Some("fp")).unwrap();
        schema::record_language_semantic_pass(&conn, "alpha").unwrap();
        schema::record_language_bulk_indexed(&conn, "beta", Some("fp")).unwrap();

        assert_eq!(count(&conn, "SELECT COUNT(*) FROM containers WHERE language = 'alpha'"), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM containers WHERE language = 'beta'"), 1);

        delete_language_rows(&mut conn, "alpha").unwrap();

        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM nodes WHERE language = 'alpha'"),
            0,
            "every alpha node, including its container node, must be gone"
        );
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM containers WHERE language = 'alpha'"),
            0,
            "alpha's container row must be gone"
        );
        // Alpha's own CALLS edge (a1 -> a2) and its two DEFINES edges
        // (container "pkg" -> a1, "pkg" -> a2) must all be gone; only beta's
        // own DEFINES edge (container "mod" -> b1), untouched by an alpha
        // delete, survives - proving the delete is scoped to alpha's edges
        // specifically, not "every edge in the index".
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM edges"),
            1,
            "only beta's own DEFINES edge may survive an alpha-scoped delete"
        );
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM edges WHERE fromId IN ('a1','a2') OR toId IN ('a1','a2')"),
            0,
            "no edge may still reference a deleted alpha node"
        );
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM language_state WHERE language = 'alpha'"),
            0,
            "alpha's language_state row must be reset"
        );

        // b1 (the seeded member) plus the container node `attach` materialized
        // for it - both carry `language = 'beta'`.
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM nodes WHERE language = 'beta'"),
            2,
            "beta's nodes must be untouched"
        );
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM containers WHERE language = 'beta'"),
            1,
            "beta's container must be untouched"
        );
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM language_state WHERE language = 'beta'"),
            1,
            "beta's language_state row must be untouched"
        );
    }

    /// The other pragma state, and the one production runs in (GM-294): with
    /// foreign keys off nothing cascades from the `nodes` delete, so every
    /// node-keyed child row of the language has to be deleted by hand or it
    /// outlives its node. The test above cannot see that - its enforced
    /// foreign keys cascade on the language's behalf. Every child table is
    /// seeded for both languages, so a delete scoped wrongly (all rows, or
    /// none) fails as clearly as a missing one.
    #[test]
    fn delete_language_rows_leaves_no_orphaned_child_rows_without_foreign_keys() {
        use crate::storage::write::PlaceholderTargetRecord;

        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        schema::apply(&conn).unwrap();

        let placeholder = |id: &str, language: &str| {
            let mut node = NodeRecord::new(id, "Module", id, id, format!("{id}.src"), language);
            node.native_kind = Some("pending_symbol".to_string());
            node.target = Some(PlaceholderTargetRecord {
                scope_kind: "file".to_string(),
                scope: "elsewhere.src".to_string(),
                key_kind: "name".to_string(),
                key: "thing".to_string(),
                from_container: None,
            });
            node
        };
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    node("a1", "alpha", "a.alpha", Some("pkg")),
                    placeholder("a-use", "alpha"),
                    node("b1", "beta", "b.beta", Some("mod")),
                    placeholder("b-use", "beta"),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        for id in ["a1", "b1"] {
            conn.execute(
                "INSERT INTO declarations (nodeId, ordinal, startLine, startCol, endLine, endCol, hasBody)
                 VALUES (?1, 0, 0, 0, 0, 1, 0)",
                params![id],
            )
            .unwrap();
            crate::storage::vectors::insert(&conn, id, &[1.0, 0.0], "test-model").unwrap();
        }

        delete_language_rows(&mut conn, "alpha").unwrap();

        for table in ["declarations", "vectors", "placeholder_targets", "containers"] {
            assert_eq!(
                count(
                    &conn,
                    &format!("SELECT COUNT(*) FROM {table} WHERE nodeId NOT IN (SELECT id FROM nodes)")
                ),
                0,
                "{table}: an alpha row outlived its node"
            );
            assert_eq!(
                count(&conn, &format!("SELECT COUNT(*) FROM {table}")),
                1,
                "{table}: beta's own row must be untouched"
            );
        }
    }

    // -----------------------------------------------------------------
    // End-to-end, through the real registry and a real spawned fixture
    // plugin process - what actually proves discrimination between
    // languages and the routing decisions in `daemon::registry`.
    // -----------------------------------------------------------------

    /// A registry over two fake languages: `alpha`, watching `go.mod` and
    /// excluding `vendor`, and `beta`, with no workspace configuration at
    /// all (the ordinary, pre-GM-272 shape) - what every test below builds
    /// on to prove one language's workspace reindex leaves the other alone.
    fn two_language_registry() -> (tempfile::TempDir, tempfile::TempDir, PathBuf, PathBuf, PluginRegistry) {
        let project = tempfile::tempdir().expect("failed to create a project root");
        let plugins = tempfile::tempdir().expect("failed to create a plugin root");

        let alpha_dir = test_plugin::install_with_workspace(
            plugins.path(),
            "alpha",
            &[".alpha-src"],
            &["go.mod"],
            &["vendor"],
        );
        let beta_dir = test_plugin::install(plugins.path(), "beta", &[".beta-src"]);

        let discovered =
            discover(&[plugins.path().to_path_buf()]).expect("the fixture manifests must discover cleanly");
        let state_dir = crate::storage::connection::project_dir(project.path())
            .expect("failed to resolve the fixture project's state directory");
        std::fs::create_dir_all(&state_dir).expect("failed to create the fixture state directory");
        let registry = PluginRegistry::new(
            project.path(),
            state_dir,
            discovered,
            None,
            None,
            Arc::new(EmbeddingPipeline::disabled()),
        );
        (project, plugins, alpha_dir, beta_dir, registry)
    }

    /// Every surviving container's `memberCount` equals its `DEFINES` edge
    /// count, and none is zero - GM-265's handoff acceptance criterion for
    /// this task, checked directly against the database rather than assumed
    /// from the deletion strategy's own reasoning.
    fn assert_container_invariants(conn: &Connection) {
        let mut stmt = conn.prepare("SELECT nodeId, memberCount FROM containers").unwrap();
        let rows: Vec<(String, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for (node_id, member_count) in rows {
            assert!(member_count > 0, "container {node_id} has memberCount {member_count}, must be > 0");
            let defines: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM edges WHERE fromId = ?1 AND kind = 'DEFINES'",
                    rusqlite::params![node_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                defines, member_count,
                "container {node_id}'s memberCount ({member_count}) disagrees with its actual \
                 DEFINES edge count ({defines})"
            );
        }
    }

    /// The task's headline acceptance criterion: touching `alpha`'s watched
    /// file (`go.mod`) reindexes `alpha` alone - its pre-existing rows are
    /// deleted and replaced by a fresh walk - while `beta`'s node ids and
    /// `language_state` row are untouched, and `beta`'s plugin is never even
    /// spawned.
    #[test]
    fn touching_the_watched_file_reindexes_only_that_language() {
        let (_project, _plugins, alpha_dir, beta_dir, registry) = two_language_registry();
        let conn = test_plugin::empty_index();

        {
            let mut guard = conn.lock().unwrap();
            schema::ensure_current(&guard, "test-generation").unwrap();
            apply_diff(
                &mut guard,
                &Diff {
                    upsert_nodes: vec![
                        node("alpha-custom", "alpha", "src/old.alpha-src", Some("pkg")),
                        node("beta-custom", "beta", "src/keep.beta-src", Some("mod")),
                    ],
                    ..Default::default()
                },
            )
            .unwrap();
            schema::record_language_bulk_indexed(&guard, "beta", Some("beta-fingerprint")).unwrap();
            schema::record_language_semantic_pass(&guard, "beta").unwrap();
        }

        let beta_state_before: (Option<String>, Option<String>, Option<String>) = {
            let guard = conn.lock().unwrap();
            guard
                .query_row(
                    "SELECT bulkIndexedAt, semanticPassAt, pluginFingerprint FROM language_state \
                     WHERE language = 'beta'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap()
        };
        {
            let guard = conn.lock().unwrap();
            assert_eq!(
                count(&guard, "SELECT COUNT(*) FROM containers WHERE language = 'alpha'"),
                1,
                "the seeded alpha node must have materialized a container"
            );
        }

        registry.route_settled_path(&conn, "go.mod".to_string());

        let guard = conn.lock().unwrap();

        // alpha: proof of actual deletion-and-rewalk, not a no-op.
        assert_eq!(
            count(&guard, "SELECT COUNT(*) FROM nodes WHERE id = 'alpha-custom'"),
            0,
            "alpha's pre-reindex node must have been deleted"
        );
        assert_eq!(
            count(&guard, "SELECT COUNT(*) FROM nodes WHERE id IN ('alpha-n1', 'alpha-n2')"),
            2,
            "alpha must have been re-walked through the bulk-index fixture"
        );
        assert_eq!(
            count(&guard, "SELECT COUNT(*) FROM containers WHERE language = 'alpha'"),
            0,
            "the old container must not survive - the re-walk's fixture output defines none"
        );

        // beta: completely untouched.
        assert_eq!(
            count(&guard, "SELECT COUNT(*) FROM nodes WHERE id = 'beta-custom'"),
            1,
            "beta's node ids must be untouched by alpha's reindex"
        );
        let beta_state_after: (Option<String>, Option<String>, Option<String>) = guard
            .query_row(
                "SELECT bulkIndexedAt, semanticPassAt, pluginFingerprint FROM language_state \
                 WHERE language = 'beta'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(beta_state_after, beta_state_before, "beta's language_state must be byte-identical");

        assert_container_invariants(&guard);
        drop(guard);

        assert!(
            test_plugin::spawns(&alpha_dir).len() >= 2,
            "alpha must have spawned both its long-lived control process and a one-shot \
             bulk-index process for the reindex: {:?}",
            test_plugin::spawns(&alpha_dir)
        );
        assert!(
            test_plugin::notifications(&alpha_dir).iter().any(|line| line == "workspaceChanged go.mod"),
            "alpha must have received the workspaceChanged notification: {:?}",
            test_plugin::notifications(&alpha_dir)
        );
        assert!(
            test_plugin::spawns(&beta_dir).is_empty(),
            "beta's plugin must never have been spawned at all"
        );
    }

    /// A `go.mod`-shaped file under `alpha`'s own `exclude_dirs = ["vendor"]`
    /// must not be routed at all - no reindex, no spawn, nothing.
    #[test]
    fn a_watched_file_under_an_excluded_directory_is_not_routed() {
        let (_project, _plugins, alpha_dir, _beta_dir, registry) = two_language_registry();
        let conn = test_plugin::empty_index();
        {
            let guard = conn.lock().unwrap();
            schema::ensure_current(&guard, "test-generation").unwrap();
        }

        registry.route_settled_path(&conn, "vendor/go.mod".to_string());

        assert!(
            test_plugin::spawns(&alpha_dir).is_empty(),
            "a watched file name under an excluded directory must never spawn the plugin"
        );
        assert!(
            test_plugin::notifications(&alpha_dir).is_empty(),
            "and must never receive the workspaceChanged notification either"
        );
    }

    /// Glob matching (`*.alpha-cfg`), distinct from the exact-name case
    /// above - this task's own explicit acceptance criterion.
    #[test]
    fn a_glob_watch_files_pattern_triggers_the_same_reindex() {
        let project = tempfile::tempdir().expect("failed to create a project root");
        let plugins = tempfile::tempdir().expect("failed to create a plugin root");
        let alpha_dir = test_plugin::install_with_workspace(
            plugins.path(),
            "alpha",
            &[".alpha-src"],
            &["*.alpha-cfg"],
            &[],
        );
        let discovered =
            discover(&[plugins.path().to_path_buf()]).expect("the fixture manifest must discover cleanly");
        let state_dir = crate::storage::connection::project_dir(project.path())
            .expect("failed to resolve the fixture project's state directory");
        std::fs::create_dir_all(&state_dir).expect("failed to create the fixture state directory");
        let registry = PluginRegistry::new(
            project.path(),
            state_dir,
            discovered,
            None,
            None,
            Arc::new(EmbeddingPipeline::disabled()),
        );
        let conn = test_plugin::empty_index();
        {
            let guard = conn.lock().unwrap();
            schema::ensure_current(&guard, "test-generation").unwrap();
        }

        registry.route_settled_path(&conn, "settings.alpha-cfg".to_string());

        assert!(
            test_plugin::spawns(&alpha_dir).len() >= 2,
            "a glob-matched workspace file must trigger the same reindex as an exact-name one: {:?}",
            test_plugin::spawns(&alpha_dir)
        );
        let guard = conn.lock().unwrap();
        assert_eq!(
            count(&guard, "SELECT COUNT(*) FROM nodes WHERE id IN ('alpha-n1', 'alpha-n2')"),
            2,
            "the glob-triggered reindex must have actually populated the graph"
        );
    }

    /// The semantic phase, restricted to the reindexed language: a
    /// `semantic_pass`-capable language gets `language_state.semanticPassAt`
    /// recorded after its workspace reindex.
    #[test]
    fn the_reindexed_languages_own_semantic_pass_runs_and_is_recorded() {
        let project = tempfile::tempdir().expect("failed to create a project root");
        let plugins = tempfile::tempdir().expect("failed to create a plugin root");
        let alpha_dir = test_plugin::install_with_workspace_semantic_pass_capable(
            plugins.path(),
            "alpha",
            &[".alpha-src"],
            &["go.mod"],
            &[],
        );
        let discovered =
            discover(&[plugins.path().to_path_buf()]).expect("the fixture manifest must discover cleanly");
        let state_dir = crate::storage::connection::project_dir(project.path())
            .expect("failed to resolve the fixture project's state directory");
        std::fs::create_dir_all(&state_dir).expect("failed to create the fixture state directory");
        let registry = PluginRegistry::new(
            project.path(),
            state_dir,
            discovered,
            None,
            None,
            Arc::new(EmbeddingPipeline::disabled()),
        );
        let conn = test_plugin::empty_index();
        {
            let guard = conn.lock().unwrap();
            schema::ensure_current(&guard, "test-generation").unwrap();
        }

        registry.route_settled_path(&conn, "go.mod".to_string());

        let guard = conn.lock().unwrap();
        let semantic_pass_at: Option<String> = guard
            .query_row("SELECT semanticPassAt FROM language_state WHERE language = 'alpha'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(
            semantic_pass_at.is_some(),
            "a semantic_pass-capable language's reindex must record its own semantic pass"
        );
        drop(guard);
        assert!(
            test_plugin::requests(&alpha_dir).iter().any(|line| line.starts_with("semanticPass")),
            "the plugin must actually have been asked for a semantic pass: {:?}",
            test_plugin::requests(&alpha_dir)
        );
    }

    /// A semantic pass that fails after a workspace reindex leaves its reason
    /// in `language_state` for status, and the language stays owed.
    #[test]
    fn a_failed_semantic_pass_after_a_workspace_reindex_records_its_reason() {
        const REASON: &str = "the language server did not answer a question about src/a.alpha-src within 10s";
        let project = tempfile::tempdir().expect("failed to create a project root");
        let plugins = tempfile::tempdir().expect("failed to create a plugin root");
        let alpha_dir = test_plugin::install_with_workspace_semantic_pass_capable(
            plugins.path(),
            "alpha",
            &[".alpha-src"],
            &["go.mod"],
            &[],
        );
        test_plugin::answer_first_semantic_pass_incomplete(&alpha_dir, "alpha", Some(REASON));
        let discovered =
            discover(&[plugins.path().to_path_buf()]).expect("the fixture manifest must discover cleanly");
        let state_dir = crate::storage::connection::project_dir(project.path())
            .expect("failed to resolve the fixture project's state directory");
        std::fs::create_dir_all(&state_dir).expect("failed to create the fixture state directory");
        let registry = PluginRegistry::new(
            project.path(),
            state_dir,
            discovered,
            None,
            None,
            Arc::new(EmbeddingPipeline::disabled()),
        );
        let conn = test_plugin::empty_index();
        {
            let guard = conn.lock().unwrap();
            schema::ensure_current(&guard, "test-generation").unwrap();
        }

        registry.route_settled_path(&conn, "go.mod".to_string());

        let guard = conn.lock().unwrap();
        let failures = schema::semantic_pass_failures(&guard).unwrap();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(failures[0].0, "alpha");
        assert!(failures[0].1.contains(REASON), "the plugin's reason is recorded: {failures:?}");
        let semantic_pass_at: Option<String> = guard
            .query_row("SELECT semanticPassAt FROM language_state WHERE language = 'alpha'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(semantic_pass_at.is_none(), "a failed pass leaves the language owed");
        drop(guard);
        assert!(
            test_plugin::requests(&alpha_dir).iter().any(|line| line.starts_with("semanticPass")),
            "the plugin must actually have been asked for a semantic pass: {:?}",
            test_plugin::requests(&alpha_dir)
        );
    }

    /// A registry with one `semantic_pass`-capable language, `alpha`, over an
    /// empty current-schema index.
    fn alpha_registry() -> (tempfile::TempDir, tempfile::TempDir, PluginRegistry, IndexStore) {
        let project = tempfile::tempdir().expect("failed to create a project root");
        let plugins = tempfile::tempdir().expect("failed to create a plugin root");
        test_plugin::install_with_workspace_semantic_pass_capable(
            plugins.path(),
            "alpha",
            &[".alpha-src"],
            &["go.mod"],
            &[],
        );
        let discovered =
            discover(&[plugins.path().to_path_buf()]).expect("the fixture manifest must discover cleanly");
        let state_dir = crate::storage::connection::project_dir(project.path())
            .expect("failed to resolve the fixture project's state directory");
        std::fs::create_dir_all(&state_dir).expect("failed to create the fixture state directory");
        let registry = PluginRegistry::new(
            project.path(),
            state_dir,
            discovered,
            None,
            None,
            Arc::new(EmbeddingPipeline::disabled()),
        );
        let conn = test_plugin::empty_index();
        schema::ensure_current(&conn.lock().unwrap(), "test-generation").unwrap();
        (project, plugins, registry, conn)
    }

    /// A reindex whose plugin is asleep when its semantic pass comes up does
    /// not wake it: the pass is recorded as not run, and the language stays
    /// owed.
    #[test]
    fn a_semantic_pass_not_run_after_a_workspace_reindex_records_why() {
        let (_project, _plugins, registry, conn) = alpha_registry();
        let supervisor = registry.get_or_spawn("alpha").expect("the fixture plugin spawns");
        supervisor.sleep_now("the test put it to sleep");

        run(&registry, &supervisor, &conn, "go.mod").expect("the reindex itself succeeds");

        let guard = conn.lock().unwrap();
        let failures = schema::semantic_pass_failures(&guard).unwrap();
        assert_eq!(failures, vec![("alpha".to_string(), semantic::NOT_RUN_REASON.to_string())]);
        let semantic_pass_at: Option<String> = guard
            .query_row("SELECT semanticPassAt FROM language_state WHERE language = 'alpha'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(semantic_pass_at.is_none(), "a pass that did not run leaves the language owed");
    }

    /// A pass that ran but whose completion could not be written after a
    /// workspace reindex is recorded as a failure with the write's error.
    #[test]
    fn a_completion_that_cannot_be_written_after_a_workspace_reindex_records_the_error() {
        let (_project, _plugins, registry, conn) = alpha_registry();
        // Refuses exactly the write that sets `semanticPassAt`; the failure's
        // own write sets only `semanticPassError`.
        conn.lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER refuse_completion_insert BEFORE INSERT ON language_state
                 WHEN NEW.semanticPassAt IS NOT NULL
                 BEGIN SELECT RAISE(ABORT, 'the disk is full'); END;
                 CREATE TRIGGER refuse_completion_update BEFORE UPDATE OF semanticPassAt ON language_state
                 WHEN NEW.semanticPassAt IS NOT NULL
                 BEGIN SELECT RAISE(ABORT, 'the disk is full'); END;",
            )
            .unwrap();

        registry.route_settled_path(&conn, "go.mod".to_string());

        let failures = schema::semantic_pass_failures(&conn.lock().unwrap()).unwrap();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(failures[0].0, "alpha");
        assert!(failures[0].1.contains("the disk is full"), "{failures:?}");
    }
}
