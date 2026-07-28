pub mod apply;
pub mod burst;
pub mod debounce;
pub mod staleness;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// Watches a project root for filesystem changes, filtering out anything
/// `.gitignore` (plus `.git` itself, which `.gitignore` files don't
/// normally list - it's special-cased the way git and `ignore`-crate
/// directory walkers already do).
pub struct ProjectWatcher {
    // Held only to keep the OS watch alive - dropping it stops watching.
    _watcher: RecommendedWatcher,
    events: Receiver<PathBuf>,
    gitignore: Gitignore,
}

impl ProjectWatcher {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        // notify's OS backends (FSEvents on macOS in particular) report
        // canonicalized paths - e.g. /var/... comes back as /private/var/...
        // since /var is a symlink. Building the gitignore matcher against a
        // non-canonical root would make every relative-path match against
        // reported events silently fail, so canonicalize once up front and
        // use that form everywhere.
        let root = root.as_ref().canonicalize().with_context(|| {
            format!("failed to canonicalize project root {}", root.as_ref().display())
        })?;
        let root = root.as_path();

        let mut builder = GitignoreBuilder::new(root);
        builder.add_line(None, ".git/").context("failed to add built-in .git exclusion")?;
        // A missing .gitignore is the common case (no ignore rules yet),
        // not an error - only propagate genuine parse failures.
        if let Some(err) = builder.add(root.join(".gitignore")) {
            if root.join(".gitignore").exists() {
                return Err(err).context("failed to parse .gitignore");
            }
        }
        let gitignore = builder.build().context("failed to build gitignore matcher")?;

        let (tx, rx) = channel();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                for path in event.paths {
                    let _ = tx.send(path);
                }
            }
        })
        .context("failed to create file watcher")?;
        watcher
            .watch(root, RecursiveMode::Recursive)
            .with_context(|| format!("failed to watch {}", root.display()))?;

        Ok(Self { _watcher: watcher, events: rx, gitignore })
    }

    fn is_ignored(&self, path: &Path) -> bool {
        // `matched` alone only tests the exact path against patterns; a
        // directory-only pattern like "node_modules/" wouldn't cover a file
        // beneath it without also checking ancestors, same as real git.
        let is_dir = path.is_dir();
        self.gitignore.matched_path_or_any_parents(path, is_dir).is_ignore()
    }

    /// Returns the next change to a non-ignored path, waiting up to
    /// `timeout`. `None` means either nothing arrived in time or the
    /// watcher was dropped.
    pub fn next_change(&self, timeout: Duration) -> Option<PathBuf> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match self.events.recv_timeout(remaining) {
                Ok(path) if self.is_ignored(&path) => continue,
                Ok(path) => return Some(path),
                Err(RecvTimeoutError::Timeout) => return None,
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
    const NO_EVENT_TIMEOUT: Duration = Duration::from_millis(500);
    const DRAIN_TIMEOUT: Duration = Duration::from_millis(300);

    /// macOS FSEvents can replay a creation event for the watched root
    /// itself (and other setup noise) shortly after `watch()` starts, even
    /// though it isn't gitignored and predates the watcher. Drain that
    /// startup noise before asserting on the write under test, or it reads
    /// as a false "change detected".
    fn drain_startup_noise(watcher: &ProjectWatcher) {
        while watcher.next_change(DRAIN_TIMEOUT).is_some() {}
    }

    #[test]
    fn write_to_tracked_file_produces_an_event() {
        let tmp = tempfile::tempdir().unwrap();
        let watcher = ProjectWatcher::new(tmp.path()).unwrap();
        drain_startup_noise(&watcher);

        let tracked = tmp.path().join("tracked.txt");
        fs::write(&tracked, b"hello").unwrap();

        let changed = watcher.next_change(EVENT_TIMEOUT);
        assert_eq!(changed, Some(tracked.canonicalize().unwrap()));
    }

    #[test]
    fn write_to_gitignored_path_produces_no_event() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".gitignore"), b"node_modules/\n").unwrap();
        fs::create_dir(tmp.path().join("node_modules")).unwrap();

        let watcher = ProjectWatcher::new(tmp.path()).unwrap();
        drain_startup_noise(&watcher);

        fs::write(tmp.path().join("node_modules/ignored.txt"), b"noise").unwrap();
        assert!(
            watcher.next_change(NO_EVENT_TIMEOUT).is_none(),
            "a write under a .gitignore'd directory must not surface as a change"
        );

        // Confirm the watcher is still alive and correctly reports real
        // changes afterward - a silent watcher isn't proof of filtering.
        fs::write(tmp.path().join("tracked.txt"), b"hello").unwrap();
        assert!(watcher.next_change(EVENT_TIMEOUT).is_some());
    }

    #[test]
    fn write_under_dot_git_produces_no_event() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();

        let watcher = ProjectWatcher::new(tmp.path()).unwrap();
        drain_startup_noise(&watcher);

        fs::write(tmp.path().join(".git/HEAD"), b"ref: refs/heads/main").unwrap();
        assert!(
            watcher.next_change(NO_EVENT_TIMEOUT).is_none(),
            ".git is always excluded even when not explicitly listed in .gitignore"
        );

        fs::write(tmp.path().join("tracked.txt"), b"hello").unwrap();
        assert!(watcher.next_change(EVENT_TIMEOUT).is_some());
    }
}
