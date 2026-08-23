pub mod apply;
pub mod burst;
pub mod debounce;
pub mod staleness;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{Config, ErrorKind as NotifyErrorKind, PollWatcher, RecommendedWatcher, RecursiveMode, Watcher};

/// How often the polling fallback (see [`ProjectWatcher::new_inner`]) rescans
/// its subtree for changes. Polling a large subtree is inherently more
/// expensive than an OS watch, so this trades some latency for not hammering
/// the filesystem - the fallback only engages for the specific subtree that
/// couldn't get a real watch, not the whole project.
const POLL_FALLBACK_INTERVAL: Duration = Duration::from_secs(2);

/// Watches a project root for filesystem changes, filtering out anything
/// `.gitignore` (plus `.git` and `.claude`, which `.gitignore` files don't
/// normally list - they're special-cased the same way the JS/TS plugin's
/// bulk-index walk hard-excludes them regardless of `.gitignore` contents).
/// `.claude` holds Claude Code's own session/worktree artifacts (e.g. full
/// project copies under `.claude/worktrees/`), never real project source.
pub struct ProjectWatcher {
    // Held only to keep the OS watch alive - dropping it stops watching.
    _watcher: RecommendedWatcher,
    // Set only when the OS watch (e.g. inotify on Linux) ran out of watch
    // capacity partway through and we fell back to polling for the subtree
    // it couldn't cover. Held for the same reason as `_watcher` - dropping
    // it stops the poll loop.
    _poll_fallback: Option<PollWatcher>,
    events: Receiver<PathBuf>,
    gitignore: Gitignore,
}

impl ProjectWatcher {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        Self::new_inner(root, None)
    }

    /// Test-only seam: lets tests simulate the exact error notify's inotify
    /// backend returns when `fs.inotify.max_user_watches` is exhausted
    /// (`ErrorKind::MaxFilesWatch`, raised on `ENOSPC` from `inotify_add_watch`),
    /// without needing to actually exhaust a real inotify instance - not
    /// reproducible on this dev platform (inotify is Linux-only) or
    /// portably/non-flakily in CI.
    #[cfg(test)]
    fn new_simulating_watch_error(root: impl AsRef<Path>, simulated_error: notify::Error) -> Result<Self> {
        Self::new_inner(root, Some(simulated_error))
    }

    fn new_inner(root: impl AsRef<Path>, simulated_watch_error: Option<notify::Error>) -> Result<Self> {
        // notify's OS backends (FSEvents on macOS in particular) report
        // canonicalized paths - e.g. /var/... comes back as /private/var/...
        // since /var is a symlink. Building the gitignore matcher against a
        // non-canonical root would make every relative-path match against
        // reported events silently fail, so canonicalize once up front and
        // use that form everywhere.
        let root = root
            .as_ref()
            .canonicalize()
            .with_context(|| format!("failed to canonicalize project root {}", root.as_ref().display()))?;
        let root = root.as_path();

        let mut builder = GitignoreBuilder::new(root);
        builder.add_line(None, ".git/").context("failed to add built-in .git exclusion")?;
        builder.add_line(None, ".claude/").context("failed to add built-in .claude exclusion")?;
        // A missing .gitignore is the common case (no ignore rules yet),
        // not an error - only propagate genuine parse failures.
        if let Some(err) = builder.add(root.join(".gitignore")) {
            if root.join(".gitignore").exists() {
                return Err(err).context("failed to parse .gitignore");
            }
        }
        let gitignore = builder.build().context("failed to build gitignore matcher")?;

        let (tx, rx) = channel();
        let mut watcher = {
            let tx = tx.clone();
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(event) = res {
                    for path in event.paths {
                        let _ = tx.send(path);
                    }
                }
            })
        }
        .context("failed to create file watcher")?;

        let watch_result = match simulated_watch_error {
            Some(err) => Err(err),
            None => watcher.watch(root, RecursiveMode::Recursive),
        };

        let poll_fallback = match watch_result {
            Ok(()) => None,
            Err(err) if matches!(err.kind, NotifyErrorKind::MaxFilesWatch) => {
                // notify's inotify backend walks the tree and adds one
                // inotify watch per directory, aborting on the first
                // ENOSPC (translated to `MaxFilesWatch`). `err.paths` names
                // the specific directory it was on when that happened -
                // everything walked before it already has a working
                // inotify watch, so only *this* subtree needs a fallback,
                // not the whole project.
                let affected = err.paths.first().cloned().unwrap_or_else(|| root.to_path_buf());
                eprintln!(
                    "g-mesh: inotify watch limit reached at {} - falling back to polling for that subtree",
                    affected.display()
                );

                let tx = tx.clone();
                let mut poll_watcher = PollWatcher::new(
                    move |res: notify::Result<notify::Event>| {
                        if let Ok(event) = res {
                            for path in event.paths {
                                let _ = tx.send(path);
                            }
                        }
                    },
                    Config::default().with_poll_interval(POLL_FALLBACK_INTERVAL),
                )
                .context("failed to create polling fallback watcher")?;
                poll_watcher
                    .watch(&affected, RecursiveMode::Recursive)
                    .with_context(|| format!("failed to poll-watch {}", affected.display()))?;
                Some(poll_watcher)
            }
            // Any other watcher error (permissions, path gone, etc.) is a
            // real failure - keep propagating it as before.
            Err(err) => return Err(err).with_context(|| format!("failed to watch {}", root.display())),
        };

        Ok(Self { _watcher: watcher, _poll_fallback: poll_fallback, events: rx, gitignore })
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
    // The polling fallback only rescans every POLL_FALLBACK_INTERVAL (2s), so
    // give it a few cycles' worth of headroom to avoid a flaky race against
    // the poll loop's own timer.
    const POLL_EVENT_TIMEOUT: Duration = Duration::from_secs(7);

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

    #[test]
    fn write_under_dot_claude_produces_no_event() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".claude/worktrees/agent-abc123")).unwrap();

        let watcher = ProjectWatcher::new(tmp.path()).unwrap();
        drain_startup_noise(&watcher);

        fs::write(tmp.path().join(".claude/worktrees/agent-abc123/index.ts"), b"stale copy").unwrap();
        assert!(
            watcher.next_change(NO_EVENT_TIMEOUT).is_none(),
            ".claude is always excluded even when not explicitly listed in .gitignore"
        );

        fs::write(tmp.path().join("tracked.txt"), b"hello").unwrap();
        assert!(watcher.next_change(EVENT_TIMEOUT).is_some());
    }

    /// Simulates the exact error notify's inotify backend raises when
    /// `fs.inotify.max_user_watches` is exhausted partway through the
    /// initial recursive watch (`ErrorKind::MaxFilesWatch`, with the
    /// offending subtree in `paths` - see notify 6.1.1's
    /// `inotify.rs::add_single_watch`, which maps `ENOSPC` from
    /// `inotify_add_watch` to exactly this). `ProjectWatcher::new` must not
    /// fail/crash on it - it should fall back to polling the affected
    /// subtree and keep delivering changes from it.
    #[test]
    fn inotify_watch_limit_error_falls_back_to_polling_the_affected_subtree() {
        let tmp = tempfile::tempdir().unwrap();
        let affected = tmp.path().join("huge-subtree");
        fs::create_dir(&affected).unwrap();
        let affected = affected.canonicalize().unwrap();

        let simulated = notify::Error::new(notify::ErrorKind::MaxFilesWatch).add_path(affected.clone());

        let watcher = ProjectWatcher::new_simulating_watch_error(tmp.path(), simulated)
            .expect("a MaxFilesWatch error must trigger a polling fallback, not a hard failure");

        fs::write(affected.join("file.txt"), b"hello").unwrap();
        let changed = watcher.next_change(POLL_EVENT_TIMEOUT);
        assert_eq!(
            changed,
            Some(affected.join("file.txt")),
            "the polling fallback must still surface changes under the subtree \
             that couldn't get a real inotify watch"
        );
    }

    /// Non-limit watcher errors are real failures and must keep propagating
    /// as before - only the specific `MaxFilesWatch` condition should engage
    /// the polling fallback.
    #[test]
    fn non_watch_limit_error_still_propagates() {
        let tmp = tempfile::tempdir().unwrap();
        let simulated = notify::Error::new(notify::ErrorKind::PathNotFound);

        let result = ProjectWatcher::new_simulating_watch_error(tmp.path(), simulated);
        assert!(result.is_err(), "a non-watch-limit error must not be silently swallowed by the fallback");
    }
}
