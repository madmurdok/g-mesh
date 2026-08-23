//! `g-mesh reindex`: throw away the current project's index and rebuild it
//! from scratch, on demand.
//!
//! This is the escape hatch for when the index is *suspected* wrong - a
//! resolver bug fixed upstream, a graph that looks stale for no reason
//! anyone can otherwise explain - and it is deliberately not a bigger version
//! of the incremental catch-up the file watcher already does. It is the same
//! "snap and rebuild from scratch" the daemon performs on its own whenever
//! [`schema::ensure_current`] finds the schema or indexer version out of
//! date: this command drives exactly that reset path
//! ([`schema::reset`](crate::storage::schema::reset)), then does the same
//! cold-start walk ([`bulk_index::run`]) the daemon would do next time it
//! started against a wiped index - just without waiting for a restart, a
//! version bump, or anything else to make that happen.
//!
//! # Why the daemon is stopped first
//!
//! A live daemon holds this project's connection open and keeps writing to
//! it - both off the file watcher and, mid-cold-start, off its own bulk walk.
//! Wiping the tables out from under either would race a write this command
//! knows nothing about. Stopping first, the same way `g-mesh stop` does,
//! guarantees this command is the only thing touching the database for as
//! long as the rebuild takes.
//!
//! Nothing has to bring a new daemon up afterwards. The next MCP call
//! bootstraps one exactly as it always does, and that daemon finds an index
//! that is already current and already fully walked - so it starts `ready`
//! immediately rather than repeating the walk this command just finished.
//!
//! # Why the rebuild ends with a semantic pass
//!
//! That last paragraph is exactly why. A daemon that starts `ready` skips its
//! cold-start branch whole, and the whole-project semantic pass lives inside
//! it - so a rebuild that stopped at the walk would hand back an index missing
//! everything only the type checker can see (`import * as ns` call edges above
//! all; `daemon::semantic` has the full list) and nothing would ever come back
//! for it. A command whose entire purpose is to repair a suspect index must
//! not leave it strictly worse than the one a zero-config cold start builds.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::cli::stop;
use crate::daemon::bulk_index::{self, BulkIndexSummary};
use crate::daemon::{manifest, registry, semantic};
use crate::embedding::EmbeddingPipeline;
use crate::storage::{connection, schema};

/// What a `reindex` actually did.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    /// Whether a daemon was serving this project and had to be stopped
    /// before the rebuild could safely touch its index.
    pub daemon_was_running: bool,
    pub summary: BulkIndexSummary,
    /// Whether the type checker got to say what the walk could not - see this
    /// module's doc comment. False when no JS/TS plugin is installed, and when
    /// the pass failed, which costs exactly the edges it was about and is
    /// reported on stderr rather than failing the rebuild.
    pub semantic_pass_ran: bool,
}

/// Rebuilds the index for the project the current directory belongs to.
pub fn run() -> Result<()> {
    let cwd = std::env::current_dir().context("failed to resolve the current directory")?;
    let outcome = reindex(&cwd)?;
    print!("{}", render(&outcome, &cwd));
    Ok(())
}

/// Wipes and rebuilds the index for `project_root`, stopping whatever daemon
/// is serving it first.
///
/// Split out from [`run`] so it can be exercised against a temporary project
/// root without taking over the process's real cwd.
pub fn reindex(project_root: &Path) -> Result<Outcome> {
    let stop_outcome = stop::stop(project_root)?;

    // Same discovery `daemon::run`'s own cold start uses - see
    // `daemon::bulk_index::run`'s doc comment for why the walk below takes
    // discovery's result directly rather than a `PluginRegistry`. Resolved
    // before the reset rather than beside the walk, because the generation
    // this index is re-stamped with is derived from it
    // (`daemon::registry::indexer_version`): stamping anything else here would
    // have the next daemon start wipe the index this command just rebuilt.
    let discovered =
        manifest::discover(&manifest::default_roots()).context("failed to discover language plugins")?;

    let conn = connection::open(project_root).context("failed to open the project's SQLite index")?;
    schema::reset(&conn, &registry::indexer_version(&discovered))
        .context("failed to reset the project's index")?;

    let canonical_root = project_root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize project root {}", project_root.display()))?;
    let conn = Arc::new(Mutex::new(conn));
    // Loaded fresh for this one-shot walk, exactly as `daemon::run` loads it
    // for a cold start - a reindex is that same walk, run early.
    let project_config = crate::config::read_project_config(project_root)
        .context("failed to read the project's config.toml")?;
    let embedding_pipeline = EmbeddingPipeline::load(&project_config.embedding);
    let summary = bulk_index::run(&canonical_root, &conn, &embedding_pipeline, &discovered)
        .context("failed to rebuild the project's index")?;
    schema::record_bulk_index(&conn.lock().unwrap())
        .context("failed to record that the project was fully reindexed")?;

    let state_dir =
        connection::project_dir(project_root).context("failed to resolve the project's state directory")?;
    let mut semantic_pass_ran = false;
    let semantic_outcome =
        semantic::run_once(&canonical_root, &state_dir, &conn, &discovered, &embedding_pipeline);
    match &semantic_outcome {
        Ok(ran) => semantic_pass_ran = *ran,
        Err(err) => eprintln!(
            "g-mesh: the semantic pass over the rebuilt index failed ({err:#}) - \
             its edges keep whatever the structural pass resolved"
        ),
    }
    // `Ok(_)` either way records `semanticPassAt` - see `daemon::semantic`'s
    // module doc. A wipe (`schema::reset` above) already cleared any previous
    // value, so this command's own attempt is what decides whether the
    // rebuilt index is left calling its semantic pass complete.
    if semantic_outcome.is_ok() {
        schema::record_semantic_pass(&conn.lock().unwrap())
            .context("failed to record that the semantic pass completed")?;
    }

    Ok(Outcome { daemon_was_running: stop_outcome.stopped_anything(), summary, semantic_pass_ran })
}

/// Renders an outcome as the text the command prints.
pub fn render(outcome: &Outcome, project_root: &Path) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "g-mesh: reindexed {}", project_root.display());
    if outcome.daemon_was_running {
        let _ = writeln!(out, "  daemon:  stopped for the rebuild - the next tool call starts a fresh one");
    }
    let _ = writeln!(
        out,
        "  index:   {} nodes, {} edges ({} imports linked, {} symbols linked)",
        outcome.summary.nodes,
        outcome.summary.edges,
        outcome.summary.linked_imports,
        outcome.summary.linked_symbols,
    );
    if outcome.semantic_pass_ran {
        let _ = writeln!(out, "  semantic: pass complete over what the walk could not resolve");
    }
    if outcome.summary.skipped_lines > 0 {
        let _ = writeln!(
            out,
            "  warning: {} unreadable line(s) were skipped - the index may be incomplete",
            outcome.summary.skipped_lines
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn a_rebuild_against_a_running_daemon_says_it_stopped_one() {
        let outcome = Outcome {
            daemon_was_running: true,
            summary: BulkIndexSummary {
                nodes: 10,
                edges: 4,
                skipped_lines: 0,
                linked_imports: 2,
                linked_symbols: 1,
            },
            semantic_pass_ran: true,
        };

        let rendered = render(&outcome, &PathBuf::from("/tmp/project"));

        assert!(rendered.contains("reindexed /tmp/project"), "{rendered}");
        assert!(rendered.contains("stopped for the rebuild"), "{rendered}");
        assert!(rendered.contains("10 nodes, 4 edges (2 imports linked, 1 symbols linked)"), "{rendered}");
    }

    #[test]
    fn a_rebuild_against_an_idle_project_says_nothing_had_to_be_stopped() {
        let outcome = Outcome {
            daemon_was_running: false,
            summary: BulkIndexSummary::default(),
            semantic_pass_ran: false,
        };

        let rendered = render(&outcome, &PathBuf::from("/tmp/project"));

        assert!(!rendered.contains("stopped for the rebuild"), "{rendered}");
        assert!(rendered.contains("0 nodes, 0 edges (0 imports linked, 0 symbols linked)"), "{rendered}");
    }

    #[test]
    fn skipped_lines_are_called_out_as_a_warning() {
        let outcome = Outcome {
            daemon_was_running: false,
            summary: BulkIndexSummary {
                nodes: 1,
                edges: 0,
                skipped_lines: 3,
                linked_imports: 0,
                linked_symbols: 0,
            },
            semantic_pass_ran: false,
        };

        let rendered = render(&outcome, &PathBuf::from("/tmp/project"));

        assert!(rendered.contains("3 unreadable line(s) were skipped"), "{rendered}");
    }
}
