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

use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::cli::stop;
use crate::daemon::bulk_index::{self, BulkIndexSummary};
use crate::storage::{connection, schema};

/// What a `reindex` actually did.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    /// Whether a daemon was serving this project and had to be stopped
    /// before the rebuild could safely touch its index.
    pub daemon_was_running: bool,
    pub summary: BulkIndexSummary,
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

    let conn = connection::open(project_root)
        .context("failed to open the project's SQLite index")?;
    schema::reset(&conn).context("failed to reset the project's index")?;

    let canonical_root = project_root.canonicalize().with_context(|| {
        format!("failed to canonicalize project root {}", project_root.display())
    })?;
    let conn = Arc::new(Mutex::new(conn));
    let summary = bulk_index::run(&canonical_root, &conn)
        .context("failed to rebuild the project's index")?;
    schema::record_bulk_index(&conn.lock().unwrap())
        .context("failed to record that the project was fully reindexed")?;

    Ok(Outcome { daemon_was_running: stop_outcome.stopped_anything(), summary })
}

/// Renders an outcome as the text the command prints.
pub fn render(outcome: &Outcome, project_root: &Path) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "g-mesh: reindexed {}", project_root.display());
    if outcome.daemon_was_running {
        let _ = writeln!(
            out,
            "  daemon:  stopped for the rebuild - the next tool call starts a fresh one"
        );
    }
    let _ = writeln!(
        out,
        "  index:   {} nodes, {} edges ({} imports linked, {} symbols linked)",
        outcome.summary.nodes,
        outcome.summary.edges,
        outcome.summary.linked_imports,
        outcome.summary.linked_symbols,
    );
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
        };

        let rendered = render(&outcome, &PathBuf::from("/tmp/project"));

        assert!(rendered.contains("reindexed /tmp/project"), "{rendered}");
        assert!(rendered.contains("stopped for the rebuild"), "{rendered}");
        assert!(rendered.contains("10 nodes, 4 edges (2 imports linked, 1 symbols linked)"), "{rendered}");
    }

    #[test]
    fn a_rebuild_against_an_idle_project_says_nothing_had_to_be_stopped() {
        let outcome = Outcome { daemon_was_running: false, summary: BulkIndexSummary::default() };

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
        };

        let rendered = render(&outcome, &PathBuf::from("/tmp/project"));

        assert!(rendered.contains("3 unreadable line(s) were skipped"), "{rendered}");
    }
}
