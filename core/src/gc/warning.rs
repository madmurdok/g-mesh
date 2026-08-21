//! The GC warning: a printed, never-acted-on heads-up that some projects
//! under `~/.g-mesh/projects/` have gone idle past `cleanup.idleThresholdDays`.
//!
//! This is purely informational. It never deletes anything - the same
//! division of labor as the rest of `gc` (see the module doc on
//! `crate::gc`): this module only surfaces the fact, `cli::clean` is the only
//! place a deletion ever happens, and only when a human explicitly asks for
//! one.
//!
//! # Only interactive commands call this
//!
//! [`maybe_print_stale_projects_warning`] is meant to be called from
//! commands a human runs at a terminal and reads the stdout of directly -
//! `status`, `init`, `clean`, `plugins list`. It must never be called from
//! `mcp-shim`, `daemon`, or anything else whose stdout is actually protocol
//! traffic read by an MCP client rather than by a person: printing an
//! unsolicited warning line into that stream would corrupt it. `cli::status`
//! is the first (and, as of this module landing, only) caller; wiring the
//! same one-line call into `init`, `clean`, and `plugins list` is a followup
//! once those commands exist.
//!
//! # Why this scans fresh every call rather than caching
//!
//! A scan is a cheap sequence of disk reads - the same one `cli::clean`'s
//! `expired` target already does, see its module doc - and interactive
//! commands are not run often enough in a tight loop for that cost to
//! matter. Caching would only risk showing a stale answer.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config;
use crate::gc::last_used;
use crate::storage::connection::projects_root;

/// One project past the idle threshold, as far as this warning cares: just
/// enough to name it in the printed list.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StaleProject {
    id: String,
    idle: Duration,
}

/// If `cleanup.enabled` in the global config (`~/.g-mesh/config.toml`,
/// [`config::read_global_config`]), scans every project directory under
/// `~/.g-mesh/projects/` and prints a warning listing the ones idle longer
/// than `cleanup.idleThresholdDays` - to stdout, since every caller of this
/// function is an interactive command whose stdout a human is reading at a
/// terminal. Prints nothing when the switch is off, or when no project is
/// past the threshold.
///
/// Never deletes anything. Points at `g-mesh clean expired` for the actual
/// cleanup, which stays a separate, explicit step.
pub fn maybe_print_stale_projects_warning() -> Result<()> {
    let config = config::read_global_config().context("failed to read the global config")?;
    if !config.cleanup.enabled {
        return Ok(());
    }

    let root = projects_root().context("failed to resolve ~/.g-mesh/projects")?;
    let threshold_days = config.cleanup.idle_threshold_days;
    let threshold = Duration::from_secs(threshold_days * 24 * 60 * 60);

    let stale = stale_projects(&root, threshold)?;
    if let Some(text) = render(&stale, threshold_days) {
        print!("{text}");
    }
    Ok(())
}

/// Every project directory under `projects_root` whose recorded idle time
/// exceeds `threshold`, sorted by id.
///
/// A project whose idle time cannot be established (no index, no readable
/// `lastUsed`) is left out - unknown idle time is never evidence of
/// idleness, the same rule `cli::clean`'s `expired` target follows. A
/// missing `projects_root` is an empty list rather than an error: nothing
/// has been indexed on this machine yet, which is not something to warn
/// about.
fn stale_projects(projects_root: &Path, threshold: Duration) -> Result<Vec<StaleProject>> {
    if !projects_root.is_dir() {
        return Ok(Vec::new());
    }

    let entries = std::fs::read_dir(projects_root)
        .with_context(|| format!("failed to read {}", projects_root.display()))?;

    let mut stale = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read {}", projects_root.display()))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(record) = last_used::read_from_project_dir(&path).unwrap_or(None) else {
            continue;
        };
        if record.idle > threshold {
            stale.push(StaleProject { id, idle: record.idle });
        }
    }

    stale.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(stale)
}

/// Renders the warning text, or `None` when there is nothing to say.
fn render(stale: &[StaleProject], threshold_days: u64) -> Option<String> {
    if stale.is_empty() {
        return None;
    }

    let mut out = String::new();
    out.push_str(&format!(
        "g-mesh: {} project(s) have been idle for more than {threshold_days} days:\n",
        stale.len()
    ));
    for project in stale {
        out.push_str(&format!("  {} ({} idle)\n", project.id, humanize_days(project.idle)));
    }
    out.push_str(
        "  run `g-mesh clean expired` to delete them, or `g-mesh clean <project-id>` for one at a time\n",
    );
    Some(out)
}

fn humanize_days(idle: Duration) -> String {
    let days = idle.as_secs() / (24 * 60 * 60);
    if days == 1 {
        "1 day".to_string()
    } else {
        format!("{days} days")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// A `~/.g-mesh/projects/` of its own, so tests never touch the
    /// developer's real one.
    struct Projects {
        dir: tempfile::TempDir,
    }

    impl Projects {
        fn new() -> Self {
            Self { dir: tempfile::tempdir().unwrap() }
        }

        fn root(&self) -> &Path {
            self.dir.path()
        }

        /// A project directory with an index whose `lastUsed` is `idle_days`
        /// old, exactly as a real one would record it.
        fn add(&self, id: &str, idle_days: u64) {
            let path = self.root().join(id);
            std::fs::create_dir_all(&path).unwrap();
            let conn = Connection::open(path.join("index.db")).unwrap();
            crate::storage::schema::ensure_current(
                &conn,
                &crate::daemon::registry::fixture_indexer_version(),
            )
            .unwrap();
            conn.execute(
                "UPDATE meta SET lastUsed = datetime('now', ?1) WHERE id = 1",
                rusqlite::params![format!("-{idle_days} days")],
            )
            .unwrap();
        }

        /// A project directory with no index at all - unknown idle time.
        fn add_empty(&self, id: &str) {
            std::fs::create_dir_all(self.root().join(id)).unwrap();
        }
    }

    const THRESHOLD: Duration = Duration::from_secs(90 * 24 * 60 * 60);

    #[test]
    fn projects_past_the_threshold_are_listed_and_fresh_ones_are_not() {
        let projects = Projects::new();
        projects.add("aaaa1111", 100);
        projects.add("bbbb2222", 10);
        projects.add("cccc3333", 91);

        let stale = stale_projects(projects.root(), THRESHOLD).unwrap();

        let ids: Vec<&str> = stale.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["aaaa1111", "cccc3333"], "sorted by id, only the two past the threshold");
    }

    #[test]
    fn a_project_with_no_readable_last_used_is_never_reported_as_stale() {
        let projects = Projects::new();
        projects.add_empty("aaaa1111");

        let stale = stale_projects(projects.root(), THRESHOLD).unwrap();

        assert!(stale.is_empty(), "unknown idle time is not evidence of staleness");
    }

    #[test]
    fn a_missing_projects_root_is_an_empty_result_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let never_created = dir.path().join("projects");

        let stale = stale_projects(&never_created, THRESHOLD).unwrap();

        assert!(stale.is_empty());
    }

    #[test]
    fn render_is_none_when_nothing_is_stale() {
        assert_eq!(render(&[], 90), None);
    }

    #[test]
    fn render_lists_every_stale_project_and_names_the_threshold_and_cleanup_command() {
        let stale = vec![
            StaleProject { id: "aaaa1111".to_string(), idle: Duration::from_secs(100 * 86400) },
            StaleProject { id: "bbbb2222".to_string(), idle: Duration::from_secs(365 * 86400) },
        ];

        let rendered = render(&stale, 90).unwrap();

        assert!(rendered.contains("2 project(s)"), "{rendered}");
        assert!(rendered.contains("90 days"), "{rendered}");
        assert!(rendered.contains("aaaa1111"), "{rendered}");
        assert!(rendered.contains("bbbb2222"), "{rendered}");
        assert!(rendered.contains("100 days"), "{rendered}");
        assert!(rendered.contains("365 days"), "{rendered}");
        assert!(rendered.contains("g-mesh clean expired"), "{rendered}");
    }
}
