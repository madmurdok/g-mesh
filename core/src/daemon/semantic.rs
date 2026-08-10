//! The whole-project semantic pass, and the one rule about when it runs: a
//! freshly built index is not finished until it has had one.
//!
//! # Why this module exists
//!
//! The pass itself lives in the JS/TS plugin (`plugins/typescript/src/semanticPass.ts`)
//! and reaches core through `watcher::apply::apply_semantic_pass`. What lives
//! here is only *when* it is asked for over a whole project - which used to be
//! one call site inside `daemon::run`'s cold-start branch, and that turned out
//! to be a bug rather than a location.
//!
//! Three commands build a complete index from nothing:
//!
//!   - `daemon::run`'s own cold start, for a project nobody pre-indexed;
//!   - `cli::init`, whose entire promise is that the index is ready when it
//!     returns;
//!   - `cli::reindex`, which rebuilds from scratch on demand.
//!
//! All three finish by recording `meta.bulkIndexedAt`, and the *next* daemon
//! start reads that record to decide it owes no walk - and, when the pass hung
//! off the walk, no semantic pass either. So a project prepared with `init` or
//! repaired with `reindex` kept a permanently structural-only graph: no
//! namespace-import call edges at all (nothing else produces them - see
//! `NamespaceMemberUse` in the plugin), re-export chains left unresolved where
//! core's own walk could not finish them, and overloaded calls left collapsed.
//! `find_callers` on a function every caller reaches through
//! `import * as ns from "./mod"` answered nothing, on an index that reported
//! itself complete.
//!
//! Hanging the pass off *whoever built the index* rather than off the daemon
//! alone is what closes that: the three paths above now produce the same graph,
//! which is the only property any of them was ever meant to have.
//!
//! # What `meta.bulkIndexedAt` still does not record
//!
//! That the pass ran. All three callers record the marker *before* asking for
//! the pass, because in the daemon that marker is what gates serving - it means
//! "the structural graph is committed and answers are real", and delaying it
//! behind a checker would put the whole point of the fast layer back to sleep.
//! init and reindex keep the same order so one marker keeps one meaning.
//!
//! The cost is a window: a build interrupted *during* the pass leaves an index
//! that calls itself complete and has no semantic layer, and nothing later goes
//! looking. Closing that needs a second recorded fact (a `meta.semanticPassAt`
//! the daemon could check independently of whether it owed a walk), which is a
//! schema change and its own ticket. Until then the escape hatch is the one it
//! already was: `g-mesh reindex`, which now ends with this pass.
//!
//! # Best-effort, everywhere
//!
//! A pass that cannot run leaves an index that is already committed and
//! serviceable - the state the project was in a moment ago. That is worth
//! reporting and never worth failing over, so both entry points here return
//! `Result` for a caller that wants to log it and neither one is on any
//! command's success path.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::daemon::manifest::DiscoveredPlugins;
use crate::daemon::plugin::{self, PluginProcess};
use crate::daemon::registry::{plugin_pid_file_name, PluginRegistry};
use crate::embedding::EmbeddingPipeline;

/// How long a one-shot plugin gets to exit on its own before it is killed -
/// the same budget `daemon::lifecycle` gives a supervised one, for the same
/// reason: a plugin that ignores its closed stdin must not hold a command open.
const PLUGIN_EXIT_GRACE: Duration = Duration::from_millis(500);

/// Runs the pass over the whole project against a registry that already
/// exists, spawning the bundled JS/TS plugin if nothing has needed it yet.
/// Returns whether it actually ran (a sleeping plugin is left asleep - see
/// [`crate::daemon::lifecycle::PluginSupervisor::semantic_pass`]).
///
/// This is `daemon::run`'s entry point: the daemon owns its registry for the
/// rest of its life, so nothing is torn down here.
///
/// Still the bundled plugin specifically, unlike the cold-start walk beside it
/// (which walks every discovered language): this pass only ever asks that one
/// plugin's semantic layer to upgrade what its structural layer could guess at.
/// Generalizing it to run once per discovered language is separate, later work.
pub fn run_with_registry(registry: &PluginRegistry, conn: &Mutex<Connection>) -> Result<bool> {
    registry
        .get_or_spawn(plugin::BUNDLED_LANGUAGE)
        .and_then(|supervisor| supervisor.semantic_pass(conn, Vec::new()))
}

/// Runs the pass over the whole project for a command that has no registry
/// and wants none afterwards: one plugin process, one pass, one shutdown.
///
/// `cli::init` and `cli::reindex` are the callers. Both have already stopped
/// whatever daemon was serving the project, so this is the only thing touching
/// its index and its `plugin-<language>.pid` file for as long as the pass
/// takes.
///
/// A `PluginProcess` rather than a [`PluginRegistry`] deliberately: the
/// registry's supervisors exist to keep a plugin *alive* between requests and
/// to log about idling it out, neither of which a one-shot command wants -
/// exactly the same reasoning that has `daemon::bulk_index` spawn its own
/// one-shot process per language rather than borrowing the daemon's.
///
/// `Ok(false)` means there was no bundled JS/TS plugin to ask, which is a
/// legitimate install (another language's plugin, or none) rather than a
/// failure.
pub fn run_once(
    canonical_root: &Path,
    state_dir: &Path,
    conn: &Mutex<Connection>,
    discovered: &DiscoveredPlugins,
    embedding: &EmbeddingPipeline,
) -> Result<bool> {
    let Some(manifest) = discovered.manifests.get(plugin::BUNDLED_LANGUAGE) else {
        return Ok(false);
    };

    let pid_file = state_dir.join(plugin_pid_file_name(plugin::BUNDLED_LANGUAGE));
    let process = PluginProcess::spawn(canonical_root, manifest, pid_file.clone())
        .with_context(|| format!("failed to start the {} plugin", plugin::BUNDLED_LANGUAGE))?;
    // The first pid is the spawner's to record (`PluginProcess::spawn`), and
    // this one is worth recording for exactly as long as the pass runs: on a
    // large project that is minutes during which the only way anything outside
    // this process - `cli::status`, `cli::stop`, a test's teardown - can name
    // the checker holding a project open is this file.
    if let Err(err) = std::fs::write(&pid_file, process.pid().to_string()) {
        eprintln!("g-mesh: failed to write plugin pid file {}: {err}", pid_file.display());
    }

    let outcome = process.semantic_pass(conn, Vec::new(), embedding);

    // Shut down whatever the pass did: a plugin left running behind a command
    // that has returned is a checker holding ~265MB with nothing reading its
    // pipes, and a pid file naming it is the exact shape `cli::stop` and
    // `cli::status` read as a crashed daemon.
    let shutdown = process.shutdown(PLUGIN_EXIT_GRACE);
    let _ = std::fs::remove_file(&pid_file);

    outcome?;
    shutdown.with_context(|| format!("the {} plugin did not shut down cleanly", plugin::BUNDLED_LANGUAGE))?;
    Ok(true)
}
