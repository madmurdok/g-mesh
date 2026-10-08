//! The per-language reindex a settled edit to one of a manifest's
//! `[plugin.workspace] watch_files` triggers (`go.mod`, `Cargo.toml`,
//! `*.csproj`...). `daemon::registry::PluginRegistry::
//! workspace_language_matches` decides *whether* a settled path triggers
//! this; this module runs it.
//!
//! A workspace file decides where module and crate boundaries are, which
//! every other file's container key (`graph::containers`) and every
//! placeholder's scope (`graph::symbol_links`, `graph::imports`) were
//! computed against, so a single-file diff cannot correct it: the language
//! is walked again from nothing, with the same one-shot `--bulk-index`
//! machinery the cold start uses, restricted to one manifest.
//!
//! # Staging, plan, swap
//!
//! Design: [ADR 0008](../../../docs/adr/0008-workspace-reindex-staging-swap.md).
//! [`run`] never deletes the language's rows up front. It marks the language
//! in `pending_reindex`, walks and links it into a fresh staging file
//! (`staging-<language>.db` in the project's state directory) through that
//! file's own `IndexStore`, plans the difference against live
//! (`storage::language_swap::plan`, live attached read-only, no live lock),
//! embeds only the texts that changed, and applies the difference to live in
//! one transaction (`storage::language_swap::swap`), which also writes the
//! language's `language_state`, reconciles both meta roll-ups and removes the
//! `pending_reindex` row. Queries see the complete old graph of the language
//! until that transaction commits and the complete new one after it.
//!
//! A reindex that fails or is killed before the swap leaves live as it was,
//! flags included, plus the `pending_reindex` row: [`resume_pending`] runs it
//! again on the next activation, and [`remove_stale_staging`] deletes a
//! staging file left behind at the next start.
//!
//! `indexed_files` is not touched: it records whether a file's bytes still
//! match what core last read, which a workspace-file edit does not change for
//! any other file.
//!
//! # Concurrency with the ordinary per-file stream
//!
//! Everything up to and including the swap runs inside
//! `PluginSupervisor::with_exclusive_access`, the lock `file_changed`,
//! `replay_pending`, `ensure_fresh` and `semantic_pass` take for their own
//! round trips to this language's plugin. An edit to one of the language's
//! files that arrives mid-reindex waits on that lock and applies to the
//! post-swap graph, so no edit lands in live between the plan and the swap.
//! Other languages never write this language's rows; the embedding backfill
//! only adds vectors, and the swap deletes vectors by node id when it runs,
//! not from the plan.
//!
//! The semantic pass runs after the lock is released, against live, through
//! the ordinary `PluginSupervisor::semantic_pass` and the per-language
//! primitives of `daemon::semantic` (not `run_with_registry`, which would ask
//! every owed language, not only this one).
//!
//! # `workspaceChanged`
//!
//! Sent (`PluginProcess::notify_workspace_changed`) only when this language's
//! supervisor is awake: nothing is woken just to be told to drop a cache it
//! does not have. Only a language with a non-empty `watch_files` reaches this
//! module, which excludes the bundled TS plugin (`watch_files = []`).
//!
//! # Debounce
//!
//! None here: `daemon::watch_and_route_once` runs every settled path through
//! `watcher::debounce::Debouncer` before it reaches `route_settled_path`, so a
//! burst of saves to one workspace file is one call into this module.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::daemon::bulk_index::{self, WalkContext};
use crate::daemon::lifecycle::PluginSupervisor;
use crate::daemon::manifest::{self, PluginManifest};
use crate::daemon::plugin;
use crate::daemon::registry::PluginRegistry;
use crate::daemon::semantic;
use crate::embedding::EmbedStats;
use crate::storage::connection;
use crate::storage::index_store::IndexStore;
use crate::storage::language_swap::{self, SwapBookkeeping};
use crate::storage::schema;

const STAGING_PREFIX: &str = "staging-";
const STAGING_SUFFIX: &str = ".db";

/// Where `language`'s staging index lives in the project's state directory.
fn staging_path(state_dir: &Path, language: &str) -> PathBuf {
    state_dir.join(format!("{STAGING_PREFIX}{language}{STAGING_SUFFIX}"))
}

/// Removes a staging file and the journal SQLite may have left beside it.
/// Best-effort: a file that cannot be removed is replaced by the next reindex.
fn remove_staging(path: &Path) {
    let journal = PathBuf::from(format!("{}-journal", path.display()));
    for file in [path, journal.as_path()] {
        if let Err(err) = std::fs::remove_file(file) {
            if err.kind() != std::io::ErrorKind::NotFound {
                crate::log_line!(
                    "g-mesh daemon: could not remove the staging index {} ({err})",
                    file.display()
                );
            }
        }
    }
}

/// Deletes every staging index in `state_dir`, all of them left by a
/// reindex that did not finish. Called once at daemon start, before any
/// reindex can run.
pub(crate) fn remove_stale_staging(state_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(state_dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(STAGING_PREFIX) && name.ends_with(STAGING_SUFFIX) {
            remove_staging(&entry.path());
        }
    }
}

/// Deletes the semantic-pending rows (ADR 0009) of languages no discovered
/// plugin runs a semantic pass for, and of languages whose pass is recorded
/// done. Called once at daemon start. Best-effort: readers ignore such rows
/// anyway.
pub(crate) fn remove_stale_semantic_pending(
    conn: &rusqlite::Connection,
    manifests: &std::collections::HashMap<String, PluginManifest>,
) {
    let capable: HashSet<String> = manifest::semantic_pass_capable_languages(manifests).into_iter().collect();
    match schema::clear_stale_semantic_pending(conn, &capable) {
        Ok(0) => {}
        Ok(cleared) => {
            crate::log_line!(
                "g-mesh daemon: cleared the stale semantic-pending rows of {cleared} language(s)"
            )
        }
        Err(err) => crate::log_line!("g-mesh daemon: could not clear stale semantic-pending rows ({err:#})"),
    }
}

/// Runs again every workspace reindex that was started and never swapped in
/// (`pending_reindex`), one language after another, each with the file that
/// triggered it. Failures are reported and leave the row for the next start.
pub(crate) fn resume_pending(registry: &PluginRegistry, store: &IndexStore) {
    let pending = match store.with(schema::pending_reindexes) {
        Ok(pending) => pending,
        Err(err) => {
            crate::log_line!("g-mesh daemon: could not read the interrupted workspace reindexes ({err:#})");
            return;
        }
    };
    for (language, trigger) in pending {
        crate::log_line!(
            "g-mesh daemon: the {language} workspace reindex after {trigger} was interrupted - rerunning it"
        );
        registry.workspace_file_changed(store, &language, &trigger);
    }
}

/// A point inside [`run_with`] where a test can look at live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    /// The staging index is walked and linked; live is untouched.
    Walked,
    /// The plan is written and its vectors computed; the swap is next.
    Planned,
    /// The swap is committed and the lock released; the semantic pass is next.
    Swapped,
}

/// Runs `language`'s whole per-language reindex against `registry`/
/// `supervisor`, in response to a settled edit of `changed_file` (one of that
/// language's own `[plugin.workspace] watch_files`). `supervisor` must be
/// `registry.get_or_spawn(language)`'s own supervisor for `language` -
/// callers reach this exclusively through
/// `PluginRegistry::workspace_file_changed`, which already guarantees that.
pub(crate) fn run(
    registry: &PluginRegistry,
    supervisor: &PluginSupervisor,
    store: &IndexStore,
    changed_file: &str,
) -> Result<()> {
    run_with(registry, supervisor, store, changed_file, &mut |_| {})
}

/// [`run`], calling `at` at each [`Stage`].
pub(crate) fn run_with(
    registry: &PluginRegistry,
    supervisor: &PluginSupervisor,
    store: &IndexStore,
    changed_file: &str,
    at: &mut dyn FnMut(Stage),
) -> Result<()> {
    let manifest = supervisor.manifest().clone();
    let language = manifest.language.as_str();
    let staging = staging_path(registry.state_dir(), language);

    let started = std::time::Instant::now();
    let embed_stats = supervisor.with_exclusive_access(|process| -> Result<EmbedStats> {
        if let Some(process) = process {
            if let Err(err) = process.notify_workspace_changed(changed_file) {
                crate::log_line!(
                    "g-mesh daemon: failed to notify the {language} plugin that {changed_file} changed \
                     ({err:#}) - it keeps whatever module/crate map it had cached, but the reindex below \
                     walks the language from scratch regardless"
                );
            }
            // The whole-project pass after the swap below is owed from here
            // on; a plugin that asked to be told starts its engine while the
            // language is re-walked.
            if !supervisor.is_semantic_suspended() {
                if let Err(err) = process.notify_prepare_semantic_pass() {
                    crate::log_line!(
                        "g-mesh daemon: could not tell the {language} plugin its semantic pass is owed ({err:#}) \
                         - it starts its engine when the pass is asked instead"
                    );
                }
            }
        }
        store
            .with(|conn| schema::mark_pending_reindex(conn, language, changed_file))
            .with_context(|| format!("failed to mark {language}'s reindex as pending"))?;
        remove_staging(&staging);
        let rebuilt = rebuild(registry, store, &manifest, &staging, at);
        remove_staging(&staging);
        rebuilt.with_context(|| format!("failed to reindex {language} after {changed_file} changed"))
    })?;
    // Outside the locked phase: it may trim the embedding cache.
    registry.embedding().finish_unit(
        &format!("workspace-reindex {language}"),
        &embed_stats,
        started.elapsed(),
    );
    at(Stage::Swapped);

    if manifest.capabilities.semantic_pass {
        let file_count = semantic::indexed_file_count(store, language);
        match supervisor.semantic_pass(store, Vec::new(), file_count) {
            Ok(true) => {
                let recorded = store.with(|conn| schema::record_language_semantic_pass(conn, language));
                if let Err(err) = recorded {
                    crate::log_line!(
                        "g-mesh daemon: failed to record {language}'s semantic pass after a workspace \
                         reindex ({err:#})"
                    );
                    semantic::record_failure(store, language, &err);
                }
            }
            // The supervisor was asleep and deliberately left that way (see
            // `PluginSupervisor::semantic_pass`). The language stays owed for
            // whoever next asks; status shows why.
            Ok(false) => semantic::record_not_run(store, language),
            Err(err) => {
                crate::log_line!(
                    "g-mesh daemon: the {language} semantic pass after a workspace reindex failed ({err:#}) - \
                     its edges keep whatever the structural pass resolved"
                );
                semantic::record_failure(store, language, &err);
            }
        }

        let capable: HashSet<String> = registry.semantic_pass_languages().into_iter().collect();
        if let Err(err) = store.with(|conn| schema::reconcile_semantic_pass_rollup(conn, &capable)) {
            crate::log_line!(
                "g-mesh daemon: failed to update the project-wide semantic-pass roll-up ({err:#})"
            );
        }
    }

    Ok(())
}

/// Walks `manifest`'s language into the staging index at `staging`, plans the
/// difference against live, computes the vectors it owes and swaps it in.
fn rebuild(
    registry: &PluginRegistry,
    store: &IndexStore,
    manifest: &PluginManifest,
    staging: &Path,
    at: &mut dyn FnMut(Stage),
) -> Result<EmbedStats> {
    let language = manifest.language.as_str();
    let live_path = store.file_path().context("a workspace reindex needs a file-backed index")?;
    let live_path = live_path.to_str().context("the index path is not valid UTF-8")?;

    // Links as the live store does: the same discovered plugins' rules.
    let staged =
        IndexStore::new(connection::open_staging(staging)?).with_link_rules(store.link_rules().clone());
    // No `walked_files`, so no baselines (see the module doc on
    // `indexed_files`), and no embedding: only the texts the plan finds
    // changed are embedded.
    let mut ctx = WalkContext::new(&staged);
    bulk_index::walk_one_language(registry.project_root(), manifest, &mut ctx)
        .with_context(|| format!("failed to walk {language} into the staging index"))?;
    staged.link_all().context("failed to link the staging index")?;
    at(Stage::Walked);

    let embedding = registry.embedding();
    let mut staged = match staged.into_inner() {
        Ok(conn) => conn,
        Err(poisoned) => poisoned.into_inner(),
    };
    // Only a swept language's pass removes what the plan keeps.
    let keep_semantic_placeholders =
        manifest.capabilities.semantic_pass && manifest.capabilities.semantic_sweep;
    let plan = language_swap::plan(
        &mut staged,
        live_path,
        language,
        embedding.embedding_version(),
        keep_semantic_placeholders,
    )?;
    drop(staged);
    let mut stats = EmbedStats::default();
    let computed = embedding.compute(&plan.to_embed, &mut stats);
    at(Stage::Planned);

    let semantic_pass_languages: HashSet<String> = registry.semantic_pass_languages().into_iter().collect();
    let fingerprint = plugin::fingerprint(manifest);
    store.swap_language(
        staging,
        Some((embedding.as_ref(), computed.as_slice())),
        &SwapBookkeeping {
            language,
            plugin_fingerprint: &fingerprint,
            semantic_pass_languages: &semantic_pass_languages,
        },
    )?;
    let counts = plan.counts;
    crate::log_line!(
        "g-mesh daemon: {language} reindex swapped in - nodes -{} +{}, edges -{} +{}, containers -{} +{}, \
         {} texts owed a vector, {} file(s) structural until the semantic pass, {} placeholder(s) kept \
         for it",
        counts.delete_nodes,
        counts.upsert_nodes,
        counts.delete_edges,
        counts.upsert_edges,
        counts.delete_containers,
        counts.upsert_containers,
        plan.to_embed.upsert_nodes.len(),
        counts.pending_files,
        counts.keep_nodes
    );
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use rusqlite::OptionalExtension;

    use super::*;
    use crate::daemon::manifest::discover;
    use crate::daemon::test_plugin;
    use crate::embedding::EmbeddingPipeline;
    use crate::storage::write::{apply_diff, Diff, NodeRecord};

    /// A file-backed index in `project`'s state directory, where the daemon
    /// keeps it: the swap attaches the live index by its path. Foreign keys
    /// on, unlike production, so an edge written before its node or a node
    /// deleted before its edges fails the test.
    fn live_index(project: &Path) -> IndexStore {
        let conn = crate::storage::connection::open(project).expect("failed to open the live index");
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::ensure_current(&conn, "test-generation").unwrap();
        IndexStore::new(conn)
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    fn node(id: &str, language: &str, file_path: &str, container: Option<&str>) -> NodeRecord {
        let mut node = NodeRecord::new(id, "Function", id, id, file_path, language);
        node.container = container.map(str::to_string);
        node
    }

    // -----------------------------------------------------------------
    // End-to-end, through the real registry and a real spawned fixture
    // plugin process - what actually proves discrimination between
    // languages and the routing decisions in `daemon::registry`.
    // -----------------------------------------------------------------

    /// A registry over two fake languages: `alpha`, watching `go.mod` and
    /// excluding `vendor`, and `beta`, with no workspace configuration at
    /// all (the ordinary shape) - what every test below builds
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
    /// count, and none is zero, checked against the database.
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
        let conn = live_index(_project.path());

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
        let conn = live_index(_project.path());
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
        let conn = live_index(project.path());
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
        let conn = live_index(project.path());
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

    /// A registry over one fake language `alpha` watching `go.mod`, installed
    /// by `install` and then adjusted by `adjust` before discovery, and a
    /// live index for its project.
    fn one_language_registry(
        install: impl FnOnce(&Path) -> PathBuf,
        adjust: impl FnOnce(&Path),
    ) -> (tempfile::TempDir, tempfile::TempDir, PathBuf, PluginRegistry, IndexStore) {
        let project = tempfile::tempdir().expect("failed to create a project root");
        let plugins = tempfile::tempdir().expect("failed to create a plugin root");
        let alpha_dir = install(plugins.path());
        adjust(&alpha_dir);
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
        let conn = live_index(project.path());
        (project, plugins, alpha_dir, registry, conn)
    }

    /// A plugin that declares `semantic_prepare` is told its pass is owed
    /// right after `workspaceChanged`, before the re-walk and the pass that
    /// follows it; one that does not is never sent the notification.
    ///
    /// Control: remove the `notify_prepare_semantic_pass` call from
    /// `run_with` and the first half fails.
    #[test]
    fn a_workspace_reindex_tells_a_preparing_plugin_before_it_rewalks() {
        let install = |root: &Path| {
            test_plugin::install_with_workspace_semantic_pass_capable(
                root,
                "alpha",
                &[".alpha-src"],
                &["go.mod"],
                &[],
            )
        };
        let (_project, plugins, _alpha_dir, registry, conn) =
            one_language_registry(install, test_plugin::declare_semantic_prepare);

        registry.route_settled_path(&conn, "go.mod".to_string());

        let frames = test_plugin::frames(plugins.path());
        let at = |frame: &str| frames.iter().position(|line| line == frame);
        let changed = at("alpha workspaceChanged").expect("workspaceChanged was sent");
        let prepare = at("alpha prepareSemanticPass").unwrap_or_else(|| panic!("no prepare: {frames:?}"));
        let pass = at("alpha semanticPass").unwrap_or_else(|| panic!("no pass: {frames:?}"));
        assert!(changed < prepare && prepare < pass, "{frames:?}");

        let (_project, plugins, _alpha_dir, registry, conn) = one_language_registry(install, |_| {});
        registry.route_settled_path(&conn, "go.mod".to_string());
        let frames = test_plugin::frames(plugins.path());
        assert!(frames.iter().any(|line| line == "alpha semanticPass"), "{frames:?}");
        assert!(
            !frames.iter().any(|line| line.ends_with("prepareSemanticPass")),
            "a plugin that did not declare semantic_prepare must never see it: {frames:?}"
        );
    }

    /// `semantic_prepare` without `semantic_pass` means nothing: no pass is
    /// owed, so the plugin is never told one is.
    ///
    /// Control: drop the `semantic_pass` half of the gate in
    /// `PluginProcess::notify_prepare_semantic_pass` and this fails.
    #[test]
    fn semantic_prepare_without_semantic_pass_sends_nothing() {
        let install = |root: &Path| {
            test_plugin::install_with_workspace(root, "alpha", &[".alpha-src"], &["go.mod"], &[])
        };
        let adjust = |dir: &Path| {
            let manifest_path = dir.join("plugin.toml");
            let mut body = std::fs::read_to_string(&manifest_path).unwrap();
            body.push_str("\n[plugin.capabilities]\nsemantic_prepare = true\n");
            std::fs::write(&manifest_path, body).unwrap();
        };
        let (_project, plugins, _alpha_dir, registry, conn) = one_language_registry(install, adjust);

        registry.route_settled_path(&conn, "go.mod".to_string());

        let frames = test_plugin::frames(plugins.path());
        assert!(frames.iter().any(|line| line == "alpha workspaceChanged"), "{frames:?}");
        assert!(!frames.iter().any(|line| line.ends_with("prepareSemanticPass")), "{frames:?}");
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
        let conn = live_index(project.path());
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
        let conn = live_index(project.path());
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

    // -----------------------------------------------------------------
    // The embedding cache through a real workspace reindex, over the fake
    // model in `embedding::pipeline::test_support`.
    // -----------------------------------------------------------------

    use crate::embedding::pipeline::test_support::{
        cache_at, fake_model_dir, fake_pipeline, fake_vector, Counters,
    };

    /// `alpha` over the fake model and a cache of its own; returns the
    /// scratch directory holding both, which must outlive the registry.
    fn alpha_registry_embedding(
        counters: &Counters,
    ) -> (tempfile::TempDir, tempfile::TempDir, tempfile::TempDir, PluginRegistry, IndexStore) {
        let project = tempfile::tempdir().expect("failed to create a project root");
        let plugins = tempfile::tempdir().expect("failed to create a plugin root");
        let scratch = tempfile::tempdir().expect("failed to create a model and cache root");
        test_plugin::install_with_workspace(plugins.path(), "alpha", &[".alpha-src"], &["go.mod"], &[]);
        let discovered =
            discover(&[plugins.path().to_path_buf()]).expect("the fixture manifest must discover cleanly");
        let state_dir = crate::storage::connection::project_dir(project.path())
            .expect("failed to resolve the fixture project's state directory");
        std::fs::create_dir_all(&state_dir).expect("failed to create the fixture state directory");
        let model_dir = fake_model_dir(&scratch.path().join("model"), "weights v1");
        let pipeline = fake_pipeline(&model_dir, Some(cache_at(scratch.path())), counters);
        let registry =
            PluginRegistry::new(project.path(), state_dir, discovered, None, None, Arc::new(pipeline));
        let conn = live_index(project.path());
        schema::ensure_current(&conn.lock().unwrap(), "test-generation").unwrap();
        (project, plugins, scratch, registry, conn)
    }

    fn give_text(project: &Path, node: &str, doc: &str) {
        std::fs::write(project.join(format!(".{node}.doc")), doc).unwrap();
        std::fs::write(project.join(format!(".{node}.sig")), format!("fn {node}()")).unwrap();
    }

    fn stored_vector(conn: &IndexStore, node_id: &str) -> Option<Vec<u8>> {
        conn.lock()
            .unwrap()
            .query_row("SELECT embedding FROM vectors WHERE nodeId = ?1", [node_id], |row| row.get(0))
            .optional()
            .unwrap()
    }

    fn packed(vector: &[f32]) -> Vec<u8> {
        vector.iter().flat_map(|value| value.to_le_bytes()).collect()
    }

    /// A workspace reindex that changes no text embeds nothing, and keeps
    /// the vectors it had, with the cache on as well.
    ///
    /// Control: embed during the staging walk and disable the lookup in
    /// `EmbeddingPipeline::compute` (treat `cache_lookup` as always `None`)
    /// -> the second reindex makes 2 calls.
    #[test]
    fn a_workspace_reindex_of_unchanged_symbols_embeds_nothing() {
        let counters = Counters::default();
        let (project, _plugins, _scratch, registry, conn) = alpha_registry_embedding(&counters);
        give_text(project.path(), "alpha-n1", "Does the first thing.");
        give_text(project.path(), "alpha-n2", "Does the second thing.");
        let supervisor = registry.get_or_spawn("alpha").expect("the fixture plugin spawns");

        run(&registry, &supervisor, &conn, "go.mod").expect("the first reindex succeeds");
        assert_eq!(counters.embeds(), 2, "a cold cache embeds both nodes");
        let before = (stored_vector(&conn, "alpha-n1"), stored_vector(&conn, "alpha-n2"));

        run(&registry, &supervisor, &conn, "go.mod").expect("the second reindex succeeds");

        assert_eq!(counters.embeds(), 2, "unchanged texts must all be cache hits");
        let after = (stored_vector(&conn, "alpha-n1"), stored_vector(&conn, "alpha-n2"));
        assert!(after.0.is_some() && after.1.is_some(), "both vectors are back after the re-walk");
        assert_eq!(after, before, "a cached vector is the same bytes as the one it replaces");
    }

    /// Editing one doc comment re-embeds exactly that node, and its stored
    /// vector is the fresh embed of the new text.
    ///
    /// Control: key the cache on the node id instead of the text (e.g. hash
    /// `node.id` in `compute`) and the edit makes 0 calls and leaves the stale
    /// vector.
    #[test]
    fn a_changed_doc_comment_is_the_only_text_embedded_again() {
        let counters = Counters::default();
        let (project, _plugins, _scratch, registry, conn) = alpha_registry_embedding(&counters);
        give_text(project.path(), "alpha-n1", "Does the first thing.");
        give_text(project.path(), "alpha-n2", "Does the second thing.");
        let supervisor = registry.get_or_spawn("alpha").expect("the fixture plugin spawns");
        run(&registry, &supervisor, &conn, "go.mod").expect("the first reindex succeeds");
        let old_n1 = stored_vector(&conn, "alpha-n1");
        let old_n2 = stored_vector(&conn, "alpha-n2");

        give_text(project.path(), "alpha-n1", "Does the first thing, differently.");
        run(&registry, &supervisor, &conn, "go.mod").expect("the second reindex succeeds");

        assert_eq!(counters.embeds(), 3, "exactly one text changed, so exactly one more call");
        let new_n1 = stored_vector(&conn, "alpha-n1").expect("the edited node has a vector");
        assert_ne!(Some(new_n1.clone()), old_n1, "the edited node's vector changes");
        assert_eq!(new_n1, packed(&fake_vector("Does the first thing, differently.\n\nfn alpha-n1()")));
        assert_eq!(stored_vector(&conn, "alpha-n2"), old_n2, "the untouched node keeps its vector");
    }

    // -----------------------------------------------------------------
    // Staging, plan and swap (ADR 0008's tests), over walks whose streams
    // the test writes (`test_plugin::set_bulk_stream`).
    // -----------------------------------------------------------------

    use crate::protocol::types::{
        EdgeKind, NodeKind, PlaceholderTarget, Position, QualifiedPath, Range, SourceTier, TargetKey,
        TargetScope, Visibility, WireDeclaration, WireEdge, WireNode,
    };
    use crate::storage::write::EdgeRecord;

    fn wire_node(id: &str, file_path: &str) -> WireNode {
        WireNode {
            id: id.to_string(),
            kind: NodeKind::Function,
            name: id.to_string(),
            qualified_name: id.to_string(),
            file_path: file_path.to_string(),
            range: Range { start: Position { line: 0, col: 0 }, end: Position { line: 1, col: 0 } },
            signature: None,
            visibility: Visibility::Public,
            doc_comment: None,
            language: "alpha".to_string(),
            native_kind: None,
            has_syntax_errors: false,
            declarations: None,
            container: None,
            container_parent: None,
            target: None,
            alias_paths: Vec::new(),
            untyped_calls: Vec::new(),
            qualified_path: None,
        }
    }

    fn file_node(file_path: &str) -> WireNode {
        WireNode { kind: NodeKind::File, ..wire_node(&format!("file:{file_path}"), file_path) }
    }

    fn wire_edge(id: &str, from: &str, to: &str) -> WireEdge {
        WireEdge {
            id: id.to_string(),
            from_id: from.to_string(),
            to_id: to.to_string(),
            kind: EdgeKind::Calls,
            source: SourceTier::Syntactic,
            engine: "tree-sitter".to_string(),
            resolved: true,
            to_declaration: None,
        }
    }

    fn json<T: serde::Serialize>(item: &T) -> String {
        serde_json::to_string(item).unwrap()
    }

    /// `alpha`, without a semantic pass, over the fake model with no cache,
    /// and a file-backed live index. The counters count the model's embeds.
    fn alpha_staging_registry(
        counters: &Counters,
        semantic_pass: bool,
    ) -> (tempfile::TempDir, tempfile::TempDir, tempfile::TempDir, PluginRegistry, IndexStore) {
        alpha_staging_registry_with(counters, semantic_pass, false)
    }

    /// [`alpha_staging_registry`], and with `semantic_sweep` the semantic
    /// pass's manifest also declares `semantic_sweep = true`.
    fn alpha_staging_registry_with(
        counters: &Counters,
        semantic_pass: bool,
        semantic_sweep: bool,
    ) -> (tempfile::TempDir, tempfile::TempDir, tempfile::TempDir, PluginRegistry, IndexStore) {
        let project = tempfile::tempdir().expect("failed to create a project root");
        let plugins = tempfile::tempdir().expect("failed to create a plugin root");
        let scratch = tempfile::tempdir().expect("failed to create a model root");
        if semantic_pass {
            let dir = test_plugin::install_with_workspace_semantic_pass_capable(
                plugins.path(),
                "alpha",
                &[".alpha-src"],
                &["go.mod"],
                &[],
            );
            if semantic_sweep {
                test_plugin::declare_semantic_sweep(&dir);
            }
        } else {
            test_plugin::install_with_workspace(plugins.path(), "alpha", &[".alpha-src"], &["go.mod"], &[]);
        }
        let discovered =
            discover(&[plugins.path().to_path_buf()]).expect("the fixture manifest must discover cleanly");
        let state_dir = crate::storage::connection::project_dir(project.path())
            .expect("failed to resolve the fixture project's state directory");
        std::fs::create_dir_all(&state_dir).expect("failed to create the fixture state directory");
        let model_dir = fake_model_dir(&scratch.path().join("model"), "weights v1");
        let pipeline = fake_pipeline(&model_dir, None, counters);
        let registry =
            PluginRegistry::new(project.path(), state_dir, discovered, None, None, Arc::new(pipeline));
        let conn = live_index(project.path());
        (project, plugins, scratch, registry, conn)
    }

    fn rows(conn: &Connection, sql: &str) -> Vec<String> {
        let mut statement = conn.prepare(sql).unwrap();
        let columns = statement.column_count();
        statement
            .query_map([], |row| {
                (0..columns)
                    .map(|i| row.get::<_, rusqlite::types::Value>(i).map(|value| format!("{value:?}")))
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map(|values| values.join("|"))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    const GRAPH_TABLES: [&str; 8] = [
        "SELECT * FROM nodes ORDER BY id",
        "SELECT * FROM edges ORDER BY id",
        "SELECT nodeId, hex(embedding), embeddingVersion FROM vectors ORDER BY nodeId",
        "SELECT * FROM containers ORDER BY nodeId",
        "SELECT * FROM declarations ORDER BY nodeId, ordinal",
        "SELECT * FROM placeholder_targets ORDER BY nodeId",
        "SELECT * FROM qualified_suffixes ORDER BY nodeId, suffix",
        "SELECT * FROM untyped_calls ORDER BY nodeId, name",
    ];
    const FLAG_TABLES: [&str; 2] =
        ["SELECT * FROM language_state ORDER BY language", "SELECT bulkIndexedAt, semanticPassAt FROM meta"];

    fn digest_of(conn: &IndexStore, queries: &[&str]) -> Vec<String> {
        let guard = conn.lock().unwrap();
        queries.iter().flat_map(|sql| rows(&guard, sql)).collect()
    }

    /// Every row of every table a reindex may write, plus the flags.
    fn digest(conn: &IndexStore) -> Vec<String> {
        digest_of(conn, &[&GRAPH_TABLES[..], &FLAG_TABLES[..]].concat())
    }

    fn pending(conn: &IndexStore) -> Vec<(String, String)> {
        conn.with(schema::pending_reindexes).unwrap()
    }

    /// Test 1: while the staging walk runs, live still answers for the
    /// language, vectors included.
    ///
    /// Control: delete the language's rows from live before the walk (the
    /// old `IndexStore::delete_language` call in `run_with`) -> not found.
    #[test]
    fn a_symbol_stays_findable_while_its_language_is_reindexed() {
        let counters = Counters::default();
        let (project, _plugins, _scratch, registry, conn) = alpha_staging_registry(&counters, false);
        give_text(project.path(), "alpha-n1", "Does the first thing.");
        let supervisor = registry.get_or_spawn("alpha").expect("the fixture plugin spawns");
        run(&registry, &supervisor, &conn, "go.mod").expect("the first reindex succeeds");

        let mut seen = None;
        run_with(&registry, &supervisor, &conn, "go.mod", &mut |stage| {
            if stage == Stage::Walked {
                let guard = conn.lock().unwrap();
                seen = Some((
                    count(&guard, "SELECT COUNT(*) FROM nodes WHERE id = 'alpha-n1'"),
                    count(&guard, "SELECT COUNT(*) FROM vectors WHERE nodeId = 'alpha-n1'"),
                ));
            }
        })
        .expect("the second reindex succeeds");

        assert_eq!(seen, Some((1, 1)), "alpha-n1 and its vector are served mid-reindex");
    }

    /// Test 2: a symbol the new walk no longer emits is gone after the swap,
    /// with every row keyed by it: its edges, declarations, placeholder
    /// target, vector and emptied container.
    ///
    /// Control: skip the plan's `plan_delete_nodes` insert in
    /// `language_swap::plan_attached` -> `alpha-n2` survives.
    #[test]
    fn a_symbol_the_new_walk_drops_is_gone_with_every_row_it_owned() {
        let counters = Counters::default();
        let (project, _plugins, _scratch, registry, conn) = alpha_staging_registry(&counters, false);
        let n1 =
            WireNode { container: Some("pkg-a".to_string()), ..wire_node("alpha-n1", "src/a.alpha-src") };
        let n2 = WireNode {
            container: Some("pkg-b".to_string()),
            doc_comment: Some("Does the second thing.".to_string()),
            declarations: Some(vec![WireDeclaration {
                ordinal: 0,
                start_line: 0,
                start_col: 0,
                end_line: 1,
                end_col: 0,
                signature: Some("fn alpha-n2()".to_string()),
                has_body: true,
            }]),
            qualified_name: "pkg::T::alpha-n2".to_string(),
            qualified_path: Some(QualifiedPath::root("pkg").child("::", "T").child("::", "alpha-n2")),
            untyped_calls: vec!["frobnicate".to_string()],
            ..wire_node("alpha-n2", "src/b.alpha-src")
        };
        let placeholder = WireNode {
            kind: NodeKind::Module,
            native_kind: Some("pending_symbol".to_string()),
            target: Some(PlaceholderTarget {
                scope: TargetScope::File("src/elsewhere.alpha-src".to_string()),
                key: TargetKey::Name("thing".to_string()),
                from_container: None,
                key_path: None,
            }),
            ..wire_node("alpha-p2", "src/b.alpha-src")
        };
        let first = [
            json(&file_node("src/a.alpha-src")),
            json(&n1),
            json(&file_node("src/b.alpha-src")),
            json(&n2),
            json(&placeholder),
            json(&wire_edge("alpha-e1", "alpha-n1", "alpha-n2")),
            json(&wire_edge("alpha-e2", "alpha-n2", "alpha-p2")),
        ];
        test_plugin::set_bulk_stream(project.path(), "alpha", &first, 0);
        let supervisor = registry.get_or_spawn("alpha").expect("the fixture plugin spawns");
        run(&registry, &supervisor, &conn, "go.mod").expect("the first reindex succeeds");
        {
            let guard = conn.lock().unwrap();
            for (table, column) in [
                ("nodes", "id"),
                ("declarations", "nodeId"),
                ("placeholder_targets", "nodeId"),
                ("vectors", "nodeId"),
                ("qualified_suffixes", "nodeId"),
                ("untyped_calls", "nodeId"),
            ] {
                assert!(
                    count(
                        &guard,
                        &format!("SELECT COUNT(*) FROM {table} WHERE {column} IN ('alpha-n2', 'alpha-p2')")
                    ) > 0,
                    "the first walk must have written {table} rows for alpha-n2"
                );
            }
        }

        test_plugin::set_bulk_stream(
            project.path(),
            "alpha",
            &[json(&file_node("src/a.alpha-src")), json(&n1)],
            0,
        );
        run(&registry, &supervisor, &conn, "go.mod").expect("the second reindex succeeds");

        let guard = conn.lock().unwrap();
        assert_eq!(
            count(
                &guard,
                "SELECT COUNT(*) FROM nodes WHERE id IN ('alpha-n2', 'alpha-p2', 'file:src/b.alpha-src')"
            ),
            0
        );
        assert_eq!(
            count(&guard, "SELECT COUNT(*) FROM edges WHERE fromId NOT IN (SELECT id FROM nodes) OR toId NOT IN (SELECT id FROM nodes)"),
            0,
            "no edge may outlive an endpoint"
        );
        for table in [
            "declarations",
            "placeholder_targets",
            "vectors",
            "containers",
            "qualified_suffixes",
            "untyped_calls",
        ] {
            assert_eq!(
                count(
                    &guard,
                    &format!("SELECT COUNT(*) FROM {table} WHERE nodeId NOT IN (SELECT id FROM nodes)")
                ),
                0,
                "{table}: a row outlived its node"
            );
        }
        assert_eq!(count(&guard, "SELECT COUNT(*) FROM containers WHERE key = 'pkg-b'"), 0, "pkg-b emptied");
        assert_eq!(count(&guard, "SELECT COUNT(*) FROM nodes WHERE id = 'alpha-n1'"), 1);
        assert_container_invariants(&guard);
    }

    /// Test 3: a symbol whose id changes (moved to another file) is never
    /// visible twice: the old row while the walk runs, the new one after.
    ///
    /// Control: walk into live instead of staging (`WalkContext::new(store)`
    /// in `rebuild`) -> two rows while held.
    #[test]
    fn a_symbol_whose_id_changes_is_never_visible_twice() {
        let counters = Counters::default();
        let (project, _plugins, _scratch, registry, conn) = alpha_staging_registry(&counters, false);
        let named = |id: &str, file: &str| WireNode { name: "moved".to_string(), ..wire_node(id, file) };
        test_plugin::set_bulk_stream(
            project.path(),
            "alpha",
            &[json(&named("alpha-old", "src/b.alpha-src"))],
            0,
        );
        let supervisor = registry.get_or_spawn("alpha").expect("the fixture plugin spawns");
        run(&registry, &supervisor, &conn, "go.mod").expect("the first reindex succeeds");

        test_plugin::set_bulk_stream(
            project.path(),
            "alpha",
            &[json(&named("alpha-new", "src/c.alpha-src"))],
            0,
        );
        let ids =
            |conn: &IndexStore| rows(&conn.lock().unwrap(), "SELECT id FROM nodes WHERE name = 'moved'");
        let mut held = None;
        run_with(&registry, &supervisor, &conn, "go.mod", &mut |stage| {
            if stage == Stage::Walked {
                held = Some(ids(&conn));
            }
        })
        .expect("the second reindex succeeds");

        assert_eq!(held, Some(vec!["Text(\"alpha-old\")".to_string()]), "only the old row while held");
        assert_eq!(ids(&conn), vec!["Text(\"alpha-new\")".to_string()], "only the new row after");
    }

    /// Test 4: a walk that fails leaves live exactly as it was, flags
    /// included, with the language marked pending; the next start's resume
    /// runs it again and clears the mark.
    ///
    /// Controls: delete the language's rows before the walk -> the digest
    /// differs; skip `schema::mark_pending_reindex` in `run_with` -> the
    /// resume finds nothing and `alpha-n3` never appears.
    #[test]
    fn an_interrupted_reindex_leaves_live_untouched_and_is_resumed() {
        let counters = Counters::default();
        let (project, _plugins, _scratch, registry, conn) = alpha_staging_registry(&counters, false);
        give_text(project.path(), "alpha-n1", "Does the first thing.");
        let supervisor = registry.get_or_spawn("alpha").expect("the fixture plugin spawns");
        run(&registry, &supervisor, &conn, "go.mod").expect("the first reindex succeeds");
        let before = digest(&conn);
        assert!(conn.with(schema::bulk_index_completed).unwrap(), "the first reindex rolls meta up");

        let partial = [json(&file_node("src/a.alpha-src")), json(&wire_node("alpha-n3", "src/a.alpha-src"))];
        test_plugin::set_bulk_stream(project.path(), "alpha", &partial, 1);
        let failed = run(&registry, &supervisor, &conn, "Cargo.toml");

        assert!(failed.is_err(), "a plugin exiting non-zero fails the reindex");
        assert_eq!(digest(&conn), before, "live, flags included, is exactly as before");
        assert_eq!(pending(&conn), vec![("alpha".to_string(), "Cargo.toml".to_string())]);
        assert!(
            !staging_path(registry.state_dir(), "alpha").exists(),
            "the staging file does not outlive the failed reindex"
        );

        test_plugin::set_bulk_stream(project.path(), "alpha", &partial, 0);
        resume_pending(&registry, &conn);

        assert!(pending(&conn).is_empty(), "the resumed reindex clears its mark");
        let guard = conn.lock().unwrap();
        assert_eq!(count(&guard, "SELECT COUNT(*) FROM nodes WHERE id = 'alpha-n3'"), 1);
        assert_eq!(count(&guard, "SELECT COUNT(*) FROM nodes WHERE id = 'alpha-n1'"), 0);
    }

    /// A staging file left by a dead daemon is deleted at the next start;
    /// the live index and anything else in the directory is not.
    #[test]
    fn stale_staging_files_are_removed_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        for name in
            ["staging-rust.db", "staging-rust.db-journal", "staging-go.db", "index.db", "plugin-rust.pid"]
        {
            std::fs::write(dir.path().join(name), "x").unwrap();
        }
        remove_stale_staging(dir.path());
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, vec!["index.db".to_string(), "plugin-rust.pid".to_string()]);
    }

    /// Test 5: a semantic pass that fails after the swap leaves both the
    /// language's and meta's `semanticPassAt` unset, although meta read
    /// complete before the reindex.
    ///
    /// Control: make `reconcile_semantic_pass_rollup` only set (drop its
    /// clearing branch) -> meta keeps the old timestamp.
    #[test]
    fn a_failed_pass_after_the_swap_leaves_no_flag_claiming_it() {
        let counters = Counters::default();
        let (project, plugins, _scratch, registry, conn) = alpha_staging_registry(&counters, true);
        test_plugin::answer_first_semantic_pass_incomplete(
            &plugins.path().join("alpha"),
            "alpha",
            Some("no answer"),
        );
        test_plugin::set_bulk_stream(
            project.path(),
            "alpha",
            &[json(&file_node("src/a.alpha-src")), json(&wire_node("alpha-n1", "src/a.alpha-src"))],
            0,
        );
        {
            let guard = conn.lock().unwrap();
            schema::record_language_bulk_indexed(&guard, "alpha", Some("old")).unwrap();
            schema::record_language_semantic_pass(&guard, "alpha").unwrap();
            guard
                .execute(
                    "UPDATE meta SET bulkIndexedAt = CURRENT_TIMESTAMP, semanticPassAt = CURRENT_TIMESTAMP",
                    [],
                )
                .unwrap();
        }

        registry.route_settled_path(&conn, "go.mod".to_string());

        let guard = conn.lock().unwrap();
        assert_eq!(count(&guard, "SELECT COUNT(*) FROM nodes WHERE id = 'alpha-n1'"), 1, "the swap landed");
        assert_eq!(schema::semantic_pass_failures(&guard).unwrap().len(), 1, "and the pass failed");
        assert_eq!(
            rows(&guard, "SELECT semanticPassAt FROM language_state WHERE language = 'alpha'"),
            vec!["Null".to_string()]
        );
        assert!(!schema::semantic_pass_completed(&guard).unwrap(), "meta must not claim the pass");
    }

    /// Test 6: with no embedding cache, an unchanged tree embeds nothing and
    /// one edited doc comment embeds exactly one text.
    ///
    /// Control: embed during the staging walk (`WalkContext { embedding:
    /// Some(..), .. }` in `rebuild`) -> the unchanged reindex embeds 2.
    #[test]
    fn only_changed_text_is_embedded_with_the_cache_off() {
        let counters = Counters::default();
        let (project, _plugins, _scratch, registry, conn) = alpha_staging_registry(&counters, false);
        give_text(project.path(), "alpha-n1", "Does the first thing.");
        give_text(project.path(), "alpha-n2", "Does the second thing.");
        let supervisor = registry.get_or_spawn("alpha").expect("the fixture plugin spawns");
        run(&registry, &supervisor, &conn, "go.mod").expect("the first reindex succeeds");
        assert_eq!(counters.embeds(), 2, "a fresh language embeds both texts");

        run(&registry, &supervisor, &conn, "go.mod").expect("the unchanged reindex succeeds");
        assert_eq!(counters.embeds(), 2, "an unchanged tree embeds nothing");

        give_text(project.path(), "alpha-n1", "Does the first thing, differently.");
        run(&registry, &supervisor, &conn, "go.mod").expect("the edited reindex succeeds");
        assert_eq!(counters.embeds(), 3, "one edited doc comment embeds one text");
        assert_eq!(
            stored_vector(&conn, "alpha-n1"),
            Some(packed(&fake_vector("Does the first thing, differently.\n\nfn alpha-n1()")))
        );
        assert_eq!(
            stored_vector(&conn, "alpha-n2"),
            Some(packed(&fake_vector("Does the second thing.\n\nfn alpha-n2()")))
        );
    }

    /// The fake semantic pass of test 7: rewrites `alpha-e1` in place onto
    /// `alpha-n3`, as the TypeScript tier upgrades an edge, and adds
    /// `alpha-sem`, an edge no structural walk emits, as the LSP bridge and
    /// the Go tier do.
    fn fake_semantic_pass(conn: &IndexStore) {
        let upsert_edges = vec![
            semantic_edge("alpha-e1", "alpha-n1", "alpha-n3"),
            semantic_edge("alpha-sem", "alpha-n2", "alpha-n3"),
        ];
        apply_diff(&mut conn.lock().unwrap(), &Diff { upsert_edges, ..Default::default() }).unwrap();
    }

    fn three_nodes() -> Vec<String> {
        vec![
            json(&file_node("src/a.alpha-src")),
            json(&wire_node("alpha-n1", "src/a.alpha-src")),
            json(&wire_node("alpha-n2", "src/a.alpha-src")),
            json(&wire_node("alpha-n3", "src/a.alpha-src")),
            json(&wire_edge("alpha-e1", "alpha-n1", "alpha-n2")),
        ]
    }

    /// Test 7: reindexing an unchanged tree, whose edges a semantic pass has
    /// already upgraded, writes only the bookkeeping rows: `language_state`,
    /// the two meta roll-ups and the `pending_reindex` delete. `alpha-e2`
    /// stays syntactic, so its staged twin is identical to live's.
    ///
    /// Controls: upsert every staged node, as a full swap would (make the
    /// live side of the node plan's `EXCEPT` select nothing) -> more changes;
    /// drop the `EXCEPT` from `plan_upsert_edges`' subquery, so the
    /// syntactic-twin branch takes `alpha-e2` although it is identical ->
    /// one more change.
    #[test]
    fn an_unchanged_tree_swaps_in_only_its_bookkeeping() {
        let counters = Counters::default();
        let (project, _plugins, _scratch, registry, conn) = alpha_staging_registry(&counters, false);
        let mut stream = three_nodes();
        stream.push(json(&wire_edge("alpha-e2", "alpha-n2", "alpha-n1")));
        test_plugin::set_bulk_stream(project.path(), "alpha", &stream, 0);
        let supervisor = registry.get_or_spawn("alpha").expect("the fixture plugin spawns");
        run(&registry, &supervisor, &conn, "go.mod").expect("the first reindex succeeds");
        fake_semantic_pass(&conn);
        let graph_before = digest_of(&conn, &GRAPH_TABLES);

        let total_changes = |conn: &IndexStore| count(&conn.lock().unwrap(), "SELECT total_changes()");
        let (mut before, mut after) = (0, 0);
        run_with(&registry, &supervisor, &conn, "go.mod", &mut |stage| match stage {
            Stage::Planned => before = total_changes(&conn),
            Stage::Swapped => after = total_changes(&conn),
            Stage::Walked => {}
        })
        .expect("the unchanged reindex succeeds");

        assert_eq!(after - before, 4, "language_state, meta twice and pending_reindex, nothing else");
        assert_eq!(digest_of(&conn, &GRAPH_TABLES), graph_before, "no graph row changed");
    }

    /// Every edge as `id|source|toId`, read while
    /// the reindex holds the swapped state (before its own semantic pass).
    fn edges_at_swap(
        registry: &PluginRegistry,
        supervisor: &PluginSupervisor,
        conn: &IndexStore,
    ) -> Vec<String> {
        let mut held = None;
        run_with(registry, supervisor, conn, "go.mod", &mut |stage| {
            if stage == Stage::Swapped {
                held = Some(rows(&conn.lock().unwrap(), "SELECT id, source, toId FROM edges ORDER BY id"));
            }
        })
        .expect("the reindex succeeds");
        held.expect("the swap was reached")
    }

    fn edge_row(id: &str, source: &str, to: &str) -> String {
        format!("Text(\"{id}\")|Text(\"{source}\")|Text(\"{to}\")")
    }

    /// A semantic edge as a pass writes it.
    fn semantic_edge(id: &str, from: &str, to: &str) -> EdgeRecord {
        let mut edge = EdgeRecord::new(id, from, to, "CALLS", "semantic", true);
        edge.engine = "fake-lsp".to_string();
        edge
    }

    /// Indexes `first`, applies a semantic pass that writes `semantic` and
    /// retracts `retracted`, then points the walk at `second`.
    fn after_a_pass(
        first: &[String],
        semantic: Vec<EdgeRecord>,
        retracted: &[&str],
        second: &[String],
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        tempfile::TempDir,
        PluginRegistry,
        IndexStore,
        Arc<PluginSupervisor>,
    ) {
        let counters = Counters::default();
        let (project, plugins, scratch, registry, conn) = alpha_staging_registry(&counters, true);
        test_plugin::set_bulk_stream(project.path(), "alpha", first, 0);
        let supervisor = registry.get_or_spawn("alpha").expect("the fixture plugin spawns");
        run(&registry, &supervisor, &conn, "go.mod").expect("the first reindex succeeds");
        apply_diff(
            &mut conn.lock().unwrap(),
            &Diff {
                upsert_edges: semantic,
                delete_edge_ids: retracted.iter().map(|id| id.to_string()).collect(),
                ..Default::default()
            },
        )
        .unwrap();
        test_plugin::set_bulk_stream(project.path(), "alpha", second, 0);
        (project, plugins, scratch, registry, conn, supervisor)
    }

    /// Test 8: a node whose row did not change keeps every outgoing edge it
    /// has in live - the pass's edge upgraded in place and retargeted, the
    /// edge the pass added - and a structural edge the pass retracted from it
    /// stays absent, although the walk emits it again.
    ///
    /// Control: leave `plan_unchanged_nodes` empty (drop its `INSERT` in
    /// `language_swap::plan_attached`) -> `alpha-e1` reads `syntactic` onto
    /// `alpha-n2`, `alpha-sem` is gone and `alpha-retracted` is back.
    #[test]
    fn an_unchanged_node_keeps_its_live_edges_across_the_swap() {
        let mut first = three_nodes();
        first.push(json(&wire_edge("alpha-retracted", "alpha-n1", "alpha-n3")));
        let (_project, _plugins, _scratch, registry, conn, supervisor) = after_a_pass(
            &first,
            vec![
                semantic_edge("alpha-e1", "alpha-n1", "alpha-n3"),
                semantic_edge("alpha-sem", "alpha-n1", "alpha-n2"),
            ],
            &["alpha-retracted"],
            &first,
        );

        assert_eq!(
            edges_at_swap(&registry, &supervisor, &conn),
            vec![edge_row("alpha-e1", "semantic", "alpha-n3"), edge_row("alpha-sem", "semantic", "alpha-n2")]
        );
    }

    /// A node whose row changed gets exactly the walk's outgoing edges: its
    /// new structural edge, and neither the semantic edge a pass added from
    /// it nor the pass's upgrade of its structural edge.
    ///
    /// Control: count every node present in both indexes as unchanged (drop
    /// the `NOT IN plan_upsert_nodes` condition of `plan_unchanged_nodes`) ->
    /// `alpha-e2` is missing and `alpha-sem` and the semantic `alpha-e3` stay.
    #[test]
    fn a_changed_node_takes_the_walks_edges() {
        let mut first = three_nodes();
        first.push(json(&wire_edge("alpha-e3", "alpha-n2", "alpha-n3")));
        // alpha-n2's code changed: it now also calls alpha-n1.
        let mut second = first.clone();
        second[2] = json(&WireNode {
            signature: Some("fn alpha-n2(x)".to_string()),
            ..wire_node("alpha-n2", "src/a.alpha-src")
        });
        second.push(json(&wire_edge("alpha-e2", "alpha-n2", "alpha-n1")));
        let (_project, _plugins, _scratch, registry, conn, supervisor) = after_a_pass(
            &first,
            vec![
                semantic_edge("alpha-sem", "alpha-n2", "alpha-n1"),
                semantic_edge("alpha-e3", "alpha-n2", "alpha-n1"),
            ],
            &[],
            &second,
        );

        assert_eq!(
            edges_at_swap(&registry, &supervisor, &conn),
            vec![
                edge_row("alpha-e1", "syntactic", "alpha-n2"),
                edge_row("alpha-e2", "syntactic", "alpha-n1"),
                edge_row("alpha-e3", "syntactic", "alpha-n3"),
            ]
        );
    }

    /// An unchanged node's live edge into a node the walk dropped goes, and
    /// the walk's edge of the same kind from that node, onto the node that
    /// took its place, is taken; the node's other live edges stay as they
    /// are.
    ///
    /// Controls: drop the `OR (EXISTS ...)` branch of `plan_upsert_edges` ->
    /// `alpha-e5` is missing; make the unchanged branch of
    /// `plan_delete_edges` delete nothing (`THEN 0`) -> the swap fails on
    /// the foreign key into the deleted `alpha-n4`.
    #[test]
    fn an_edge_into_a_dropped_node_is_replaced_by_the_walks() {
        let mut first = three_nodes();
        first.push(json(&wire_node("alpha-n4", "src/a.alpha-src")));
        first.push(json(&wire_edge("alpha-e4", "alpha-n1", "alpha-n4")));
        let mut second = three_nodes();
        second.push(json(&wire_node("alpha-n5", "src/a.alpha-src")));
        second.push(json(&wire_edge("alpha-e5", "alpha-n1", "alpha-n5")));
        let (_project, _plugins, _scratch, registry, conn, supervisor) =
            after_a_pass(&first, vec![semantic_edge("alpha-e1", "alpha-n1", "alpha-n3")], &[], &second);

        assert_eq!(
            edges_at_swap(&registry, &supervisor, &conn),
            vec![edge_row("alpha-e1", "semantic", "alpha-n3"), edge_row("alpha-e5", "syntactic", "alpha-n5")]
        );
    }

    /// Test 12: an unchanged node's live syntactic edge whose staged twin
    /// differs (a manifest edit moved where it links) is taken from staging,
    /// while the node's semantic edges, the one a pass upgraded in place
    /// included, and a structural edge the pass retracted stay as live has
    /// them.
    ///
    /// Control: drop the syntactic-twin branch (`OR EXISTS (... t.source =
    /// 'syntactic')`) of `plan_upsert_edges` -> `alpha-e2` still reads
    /// `alpha-n2`.
    #[test]
    fn a_differing_staged_syntactic_twin_replaces_lives() {
        let mut first = three_nodes();
        first.push(json(&wire_edge("alpha-e2", "alpha-n1", "alpha-n2")));
        first.push(json(&wire_edge("alpha-retracted", "alpha-n1", "alpha-n2")));
        let mut second = three_nodes();
        second.push(json(&wire_edge("alpha-e2", "alpha-n1", "alpha-n3")));
        second.push(json(&wire_edge("alpha-retracted", "alpha-n1", "alpha-n3")));
        // alpha-e1 differs from its staged twin too, but only as the pass's
        // upgrade: the walk emits it onto alpha-n2 in both streams.
        let (_project, _plugins, _scratch, registry, conn, supervisor) = after_a_pass(
            &first,
            vec![
                semantic_edge("alpha-e1", "alpha-n1", "alpha-n3"),
                semantic_edge("alpha-sem", "alpha-n1", "alpha-n2"),
            ],
            &["alpha-retracted"],
            &second,
        );

        assert_eq!(
            edges_at_swap(&registry, &supervisor, &conn),
            vec![
                edge_row("alpha-e1", "semantic", "alpha-n3"),
                edge_row("alpha-e2", "syntactic", "alpha-n3"),
                edge_row("alpha-sem", "semantic", "alpha-n2"),
            ]
        );
    }

    /// Indexes `three_nodes` for `alpha`, whose manifest declares
    /// `semantic_sweep` or not, adds a semantic edge no pass will re-send,
    /// then runs a complete whole-project pass (the fake plugin answers with
    /// an empty diff) and returns every edge as `id|source|toId`.
    fn edges_after_a_complete_pass(semantic_sweep: bool) -> Vec<String> {
        let counters = Counters::default();
        let (project, _plugins, _scratch, registry, conn) =
            alpha_staging_registry_with(&counters, true, semantic_sweep);
        test_plugin::set_bulk_stream(project.path(), "alpha", &three_nodes(), 0);
        let supervisor = registry.get_or_spawn("alpha").expect("the fixture plugin spawns");
        run(&registry, &supervisor, &conn, "go.mod").expect("the first reindex succeeds");
        let upsert_edges = vec![semantic_edge("alpha-stale", "alpha-n2", "alpha-n3")];
        apply_diff(&mut conn.lock().unwrap(), &Diff { upsert_edges, ..Default::default() }).unwrap();

        assert!(supervisor.semantic_pass(&conn, Vec::new(), 0).expect("the pass succeeds"), "the pass ran");
        let edges = rows(&conn.lock().unwrap(), "SELECT id, source, toId FROM edges ORDER BY id");
        edges
    }

    /// Test 13: a language whose manifest declares `semantic_sweep` loses
    /// the semantic edge a complete whole-project pass did not re-send.
    ///
    /// Control: pass `None` instead of the manifest's language to
    /// `apply_semantic_pass` in `PluginProcess::semantic_pass` ->
    /// `alpha-stale` survives.
    #[test]
    fn a_complete_pass_sweeps_a_language_that_declares_the_sweep() {
        assert_eq!(edges_after_a_complete_pass(true), vec![edge_row("alpha-e1", "syntactic", "alpha-n2")]);
    }

    /// Test 13, the other arm: a language whose manifest leaves `semantic_sweep` off (as
    /// TypeScript's does) keeps it.
    ///
    /// Control: pass `Some(&self.manifest.language)` unconditionally to
    /// `apply_semantic_pass` in `PluginProcess::semantic_pass` ->
    /// `alpha-stale` is gone.
    #[test]
    fn a_complete_pass_sweeps_nothing_for_a_language_that_does_not_declare_it() {
        assert_eq!(
            edges_after_a_complete_pass(false),
            vec![
                edge_row("alpha-e1", "syntactic", "alpha-n2"),
                edge_row("alpha-stale", "semantic", "alpha-n3")
            ]
        );
    }

    // -----------------------------------------------------------------
    // Placeholders a semantic pass added, across a swap and the pass after it.
    // -----------------------------------------------------------------

    /// A placeholder as a semantic tier adds it: a pending-symbol `Module`
    /// under an id no walk emits, addressing a declaration nothing indexes,
    /// so linking leaves the pass's edge on it.
    fn semantic_placeholder(id: &str) -> WireNode {
        WireNode {
            kind: NodeKind::Module,
            native_kind: Some("pending_symbol".to_string()),
            target: Some(PlaceholderTarget {
                scope: TargetScope::File("src/elsewhere.alpha-src".to_string()),
                key: TargetKey::QualifiedName("elsewhere::gone".to_string()),
                from_container: None,
                key_path: None,
            }),
            ..wire_node(id, "src/a.alpha-src")
        }
    }

    /// The pass's answer: `alpha-sp` and a semantic edge onto it from the
    /// unchanged `alpha-n1`.
    fn a_pass_adding_a_placeholder() -> String {
        let edge = WireEdge {
            source: SourceTier::Semantic,
            engine: "fake-lsp".to_string(),
            resolved: false,
            ..wire_edge("alpha-sem", "alpha-n1", "alpha-sp")
        };
        format!(
            r#"{{"upsertNodes":[{}],"upsertEdges":[{}]}}"#,
            json(&semantic_placeholder("alpha-sp")),
            json(&edge)
        )
    }

    /// What one reindex of an unchanged tree leaves, for `alpha` with a
    /// semantic pass (and `semantic_sweep` when asked) whose earlier pass
    /// added `alpha-sp`: the `alpha-sp` node and `alpha-sem` edge rows at the
    /// swap, and after the reindex's own pass, which re-sends them only when
    /// `resend` is set. The walk streams `extra` besides `three_nodes`, in the
    /// first walk only.
    fn placeholder_rows_around_a_reindex(
        semantic_sweep: bool,
        resend: bool,
        extra: &[String],
    ) -> (Vec<String>, Vec<String>) {
        let counters = Counters::default();
        let (project, plugins, _scratch, registry, conn) =
            alpha_staging_registry_with(&counters, true, semantic_sweep);
        let plugin_dir = plugins.path().join("alpha");
        let mut first = three_nodes();
        first.extend(extra.iter().cloned());
        test_plugin::set_bulk_stream(project.path(), "alpha", &first, 0);
        test_plugin::set_semantic_pass_answer(&plugin_dir, Some(&a_pass_adding_a_placeholder()));
        let supervisor = registry.get_or_spawn("alpha").expect("the fixture plugin spawns");
        run(&registry, &supervisor, &conn, "go.mod").expect("the first reindex succeeds");
        let placeholder_rows = |conn: &IndexStore| {
            rows(
                &conn.lock().unwrap(),
                "SELECT id FROM nodes WHERE id IN ('alpha-sp', 'alpha-n4')
                 UNION ALL SELECT id FROM edges WHERE id = 'alpha-sem' ORDER BY 1",
            )
        };
        assert_eq!(
            placeholder_rows(&conn).len(),
            2 + usize::from(!extra.is_empty()),
            "the first pass added the placeholder and its edge"
        );

        test_plugin::set_bulk_stream(project.path(), "alpha", &three_nodes(), 0);
        if !resend {
            test_plugin::set_semantic_pass_answer(&plugin_dir, None);
        }
        let mut at_swap = None;
        run_with(&registry, &supervisor, &conn, "go.mod", &mut |stage| {
            if stage == Stage::Swapped {
                at_swap = Some(placeholder_rows(&conn));
            }
        })
        .expect("the second reindex succeeds");
        (at_swap.expect("the swap was reached"), placeholder_rows(&conn))
    }

    fn text(id: &str) -> String {
        format!("Text(\"{id}\")")
    }

    /// A placeholder a semantic pass added, and the pass's edge onto it,
    /// survive the swap of an unchanged tree: the walk never emits them, so
    /// staging lacking them says nothing about them. The pass after the swap
    /// re-sends both, so they stay.
    ///
    /// Controls: drop `AND id NOT IN (SELECT id FROM plan_keep_nodes)` from
    /// `plan_delete_nodes` in `language_swap::plan_attached` -> the swap fails
    /// on the foreign key from the kept `alpha-sem` into the deleted
    /// `alpha-sp` (production runs without foreign keys: both gone at the
    /// swap); drop `store.claim(diff)` from `Writer::apply_diff_linked`
    /// -> the rows are gone after the pass.
    #[test]
    fn a_semantic_placeholder_and_its_edge_survive_an_unchanged_swap() {
        let (at_swap, after) = placeholder_rows_around_a_reindex(true, true, &[]);
        assert_eq!(at_swap, vec![text("alpha-sem"), text("alpha-sp")], "kept at the swap");
        assert_eq!(after, vec![text("alpha-sem"), text("alpha-sp")], "re-sent by the pass, so still there");
    }

    /// The same placeholder goes once a complete whole-project pass no longer
    /// sends it: kept at the swap, deleted with its edge after the pass.
    ///
    /// Control: remove the `store.sweep_unclaimed_nodes(language)` call from
    /// `watcher::apply::apply_semantic_pass_in` -> `alpha-sp` is still there
    /// after the pass (its edge goes with the edge sweep either way).
    #[test]
    fn a_kept_placeholder_the_next_complete_pass_does_not_send_is_deleted() {
        let (at_swap, after) = placeholder_rows_around_a_reindex(true, false, &[]);
        assert_eq!(at_swap, vec![text("alpha-sem"), text("alpha-sp")], "kept at the swap");
        assert!(after.is_empty(), "swept after the pass: {after:?}");
    }

    /// A language whose pass is not swept keeps the old rule: nothing would
    /// ever remove a kept placeholder, so the swap deletes it and the pass
    /// adds it back.
    ///
    /// Control: pass `true` for `keep_semantic_placeholders` in `rebuild`
    /// regardless of `semantic_sweep` -> `alpha-sp` is there at the swap.
    #[test]
    fn a_language_without_the_sweep_drops_the_placeholder_at_the_swap() {
        let (at_swap, after) = placeholder_rows_around_a_reindex(false, true, &[]);
        assert!(at_swap.is_empty(), "deleted at the swap: {at_swap:?}");
        assert_eq!(after, vec![text("alpha-sem"), text("alpha-sp")], "added back by the pass");
    }

    /// Only placeholders are kept: a declaration the walk no longer emits is
    /// deleted at the swap even for a swept language.
    ///
    /// Control: drop `AND kind = 'Module' AND nativeKind = ...` from
    /// `plan_keep_nodes` in `language_swap::plan_attached` -> `alpha-n4` is
    /// there at the swap.
    #[test]
    fn a_declaration_the_walk_dropped_is_deleted_at_the_swap_all_the_same() {
        let (at_swap, _) =
            placeholder_rows_around_a_reindex(true, true, &[json(&wire_node("alpha-n4", "src/a.alpha-src"))]);
        assert_eq!(
            at_swap,
            vec![text("alpha-sem"), text("alpha-sp")],
            "alpha-n4 is gone, the placeholder kept"
        );
    }

    /// A `pending_reindex` row whose language's plugin was removed does not
    /// outlive the next daemon start, so `g-mesh status` stops naming it: the
    /// indexer generation digests every discovered plugin
    /// (`registry::indexer_version`), so removing one changes it and
    /// `schema::ensure_current` wipes the index, that row included. With the
    /// same plugins the row stays for `resume_pending`.
    ///
    /// Control: drop `DROP TABLE IF EXISTS pending_reindex;` from
    /// `schema::wipe` -> `beta`'s row survives the start without its plugin.
    #[test]
    fn a_pending_reindex_of_a_removed_plugin_goes_at_the_next_start() {
        let plugins = tempfile::tempdir().expect("failed to create a plugin root");
        test_plugin::install_with_workspace(plugins.path(), "alpha", &[".alpha-src"], &["go.mod"], &[]);
        let beta =
            test_plugin::install_with_workspace(plugins.path(), "beta", &[".beta-src"], &["go.work"], &[]);
        let generation = || {
            let discovered =
                discover(&[plugins.path().to_path_buf()]).expect("the fixtures discover cleanly");
            crate::daemon::registry::indexer_version(&discovered)
        };
        let conn = Connection::open_in_memory().unwrap();
        schema::ensure_current(&conn, &generation()).unwrap();
        schema::mark_pending_reindex(&conn, "beta", "go.work").unwrap();

        assert!(!schema::ensure_current(&conn, &generation()).unwrap(), "the same plugins keep the index");
        assert_eq!(
            schema::pending_reindexes(&conn).unwrap(),
            vec![("beta".to_string(), "go.work".to_string())],
            "a start with beta's plugin still installed resumes it"
        );

        std::fs::remove_dir_all(&beta).unwrap();
        assert!(schema::ensure_current(&conn, &generation()).unwrap(), "a removed plugin wipes the index");
        assert!(schema::pending_reindexes(&conn).unwrap().is_empty(), "beta's row went with the wipe");
    }

    // -----------------------------------------------------------------
    // Semantic-pending rows (ADR 0009) across the reindex's own pass.
    // -----------------------------------------------------------------

    /// `(language rows, file rows)` in `semantic_pending`.
    fn semantic_pending_counts(conn: &Connection) -> (i64, i64) {
        (
            count(conn, "SELECT COUNT(*) FROM semantic_pending"),
            count(conn, "SELECT COUNT(*) FROM semantic_pending_files"),
        )
    }

    /// The pending counts at `Stage::Swapped` of one reindex, and after it.
    fn semantic_pending_around_the_pass(
        registry: &PluginRegistry,
        supervisor: &PluginSupervisor,
        conn: &IndexStore,
    ) -> ((i64, i64), (i64, i64)) {
        let mut at_swap = None;
        run_with(registry, supervisor, conn, "go.mod", &mut |stage| {
            if stage == Stage::Swapped {
                at_swap = Some(semantic_pending_counts(&conn.lock().unwrap()));
            }
        })
        .expect("the reindex succeeds");
        (at_swap.expect("the swap was reached"), semantic_pending_counts(&conn.lock().unwrap()))
    }

    /// An unchanged tree still owes the pass for unchanged call sites whose
    /// dependencies moved: the language row is written with no files, and
    /// the completed pass clears it. Control: write the language row only
    /// when the plan has pending files -> no row at the swap.
    #[test]
    fn an_unchanged_reindex_still_owes_the_language_level_fact() {
        let counters = Counters::default();
        let (project, _plugins, _scratch, registry, conn) = alpha_staging_registry(&counters, true);
        test_plugin::set_bulk_stream(project.path(), "alpha", &three_nodes(), 0);
        let supervisor = registry.get_or_spawn("alpha").expect("the fixture plugin spawns");
        run(&registry, &supervisor, &conn, "go.mod").expect("the first reindex succeeds");

        let (at_swap, after) = semantic_pending_around_the_pass(&registry, &supervisor, &conn);

        assert_eq!(at_swap, (1, 0), "one language row, no files");
        assert_eq!(after, (0, 0), "the completed pass cleared it");
    }

    /// A pass not run (its plugin asleep) clears the rows the swap wrote: no
    /// pass is working on them. Control: remove the clear from
    /// `schema::record_language_semantic_pass_failure` -> rows remain.
    #[test]
    fn a_pass_not_run_after_the_swap_clears_the_pending_rows() {
        let (_project, _plugins, registry, conn) = alpha_registry();
        let supervisor = registry.get_or_spawn("alpha").expect("the fixture plugin spawns");
        supervisor.sleep_now("the test put it to sleep");

        let (at_swap, after) = semantic_pending_around_the_pass(&registry, &supervisor, &conn);

        assert_eq!(at_swap.0, 1, "the swap wrote the language row");
        assert_eq!(after, (0, 0));
    }

    /// An incomplete pass is recorded as a failure and clears the rows the
    /// same way. Control: as above.
    #[test]
    fn an_incomplete_pass_after_the_swap_clears_the_pending_rows() {
        let counters = Counters::default();
        let (project, plugins, _scratch, registry, conn) = alpha_staging_registry(&counters, true);
        test_plugin::answer_first_semantic_pass_incomplete(
            &plugins.path().join("alpha"),
            "alpha",
            Some("no answer"),
        );
        test_plugin::set_bulk_stream(project.path(), "alpha", &three_nodes(), 0);
        let supervisor = registry.get_or_spawn("alpha").expect("the fixture plugin spawns");

        let (at_swap, after) = semantic_pending_around_the_pass(&registry, &supervisor, &conn);

        assert_eq!(at_swap, (1, 1), "the new file is pending at the swap");
        assert_eq!(after, (0, 0));
        assert_eq!(schema::semantic_pass_failures(&conn.lock().unwrap()).unwrap().len(), 1);
    }

    /// A daemon killed during the pass: the rows survive the restart, the
    /// startup cleanup removes those of a language no capable plugin runs
    /// and of a language whose pass is done, and the activation retry's pass
    /// clears the owed one. Control: make `remove_stale_semantic_pending` a
    /// no-op -> the removed plugin's rows remain.
    #[test]
    fn pending_rows_survive_a_restart_until_the_retry_pass() {
        let counters = Counters::default();
        let (project, plugins, _scratch, registry, conn) = alpha_staging_registry(&counters, true);
        test_plugin::set_bulk_stream(project.path(), "alpha", &three_nodes(), 0);
        let supervisor = registry.get_or_spawn("alpha").expect("the fixture plugin spawns");
        run(&registry, &supervisor, &conn, "go.mod").expect("the reindex succeeds");
        // The state a daemon killed between the swap and the pass's record
        // leaves, plus rows of a removed plugin and of a done language.
        conn.lock()
            .unwrap()
            .execute_batch(
                "UPDATE language_state SET semanticPassAt = NULL WHERE language = 'alpha';
                 UPDATE meta SET semanticPassAt = NULL;
                 INSERT INTO semantic_pending (language, since) VALUES
                     ('alpha', '2026-09-26T10:14:03Z'), ('gone', '2026-09-26T10:14:03Z'),
                     ('beta', '2026-09-26T10:14:03Z');
                 INSERT INTO semantic_pending_files (language, filePath) VALUES
                     ('alpha', 'src/a.alpha-src'), ('gone', 'x.gone'), ('beta', 'b.beta');
                 INSERT INTO language_state (language, semanticPassAt) VALUES ('beta', CURRENT_TIMESTAMP);",
            )
            .unwrap();
        drop(conn);

        let conn = live_index(project.path());
        assert_eq!(semantic_pending_counts(&conn.lock().unwrap()), (3, 3), "the rows are on disk");
        let discovered = discover(&[plugins.path().to_path_buf()]).unwrap();
        remove_stale_semantic_pending(&conn.lock().unwrap(), &discovered.manifests);
        assert_eq!(
            rows(
                &conn.lock().unwrap(),
                "SELECT language FROM semantic_pending UNION ALL SELECT language FROM semantic_pending_files"
            ),
            vec!["Text(\"alpha\")".to_string(), "Text(\"alpha\")".to_string()]
        );

        semantic::run_with_registry(&registry, &conn);

        assert_eq!(semantic_pending_counts(&conn.lock().unwrap()), (0, 0), "the retry pass cleared it");
    }
}
