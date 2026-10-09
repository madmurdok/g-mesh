pub mod apply;
pub mod batch;
pub mod burst;
pub mod debounce;
pub mod ignore_layers;
pub mod staleness;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::RwLock;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{Config, ErrorKind as NotifyErrorKind, PollWatcher, RecommendedWatcher, RecursiveMode, Watcher};

use crate::project_walk::Remap;
use ignore_layers::{IgnoreLayers, GITIGNORE_FILE_NAME};

/// How often the polling fallback (see [`ProjectWatcher::new_inner`]) rescans
/// its subtree for changes. Polling a large subtree is inherently more
/// expensive than an OS watch, so this trades some latency for not hammering
/// the filesystem - the fallback only engages for the specific subtree that
/// couldn't get a real watch, not the whole project.
const POLL_FALLBACK_INTERVAL: Duration = Duration::from_secs(2);

/// Directory names excluded whatever `.gitignore` says, in every language:
/// git's object store and Claude Code's session directory (`.claude/worktrees/`
/// holds whole copies of the project), neither of which is ever source. The
/// same pair as `BASELINE_EXCLUDED_DIRS` in plugins/sdk/src/walk.rs, which the
/// SDK-built plugins' bulk walks apply; everything language-specific
/// (`node_modules`, `target`, `vendor`) is the plugin's own `[plugin.workspace]
/// exclude_dirs`. Shared with `cli::status`'s coverage walk.
pub const BASELINE_EXCLUDED_DIRS: [&str; 2] = [".git", ".claude"];

/// Watches a project root for filesystem changes, filtering out anything
/// a `.gitignore` at any level ignores (plus `.git` and `.claude`, which
/// `.gitignore` files don't normally list - they're special-cased the same
/// way the JS/TS plugin's bulk-index walk hard-excludes them regardless of
/// `.gitignore` contents).
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
    root: PathBuf,
    // The layers and the link table, loaded by one walk. Swapped whole by
    // `reload_ignores` when a `.gitignore` or a link may have changed; read
    // for every event.
    ignores: RwLock<IgnoreLayers>,
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

        let ignores = RwLock::new(IgnoreLayers::load(root));

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
                crate::log_line!(
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

        Ok(Self {
            _watcher: watcher,
            _poll_fallback: poll_fallback,
            events: rx,
            root: root.to_path_buf(),
            ignores,
        })
    }

    /// Whether the project's walks would leave `path` out, by the layers as
    /// last (re)loaded: see [`IgnoreLayers::is_ignored`].
    pub fn is_ignored(&self, path: &Path) -> bool {
        self.ignores.read().unwrap_or_else(|poisoned| poisoned.into_inner()).is_ignored(path)
    }

    /// Re-reads every `.gitignore` under the root and the link table, so
    /// later events (and [`Self::retain_unignored`]) are remapped and
    /// filtered by what the tree says now. Returns the project-relative
    /// spellings of the links whose row changed (created, removed,
    /// retargeted, judged differently). The read happens outside the lock;
    /// only the swap holds it.
    pub fn reload_ignores(&self) -> Vec<String> {
        let layers = IgnoreLayers::load(&self.root);
        let mut current = self.ignores.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed = current.links().changed_links(layers.links());
        *current = layers;
        changed
    }

    /// Reloads the layers and the link table when a settled batch may have
    /// changed them: a path named `.gitignore` (edited, created or deleted), a
    /// directory (a move can carry a nested `.gitignore` without an event for
    /// the file itself), a symlink, or a link spelling in the current table or
    /// an ancestor of one (a removed link is neither a directory nor a
    /// symlink any more). The root itself does not count: macOS reports it
    /// for any write inside. Returns `None` without a reload, else the
    /// changed link spellings ([`Self::reload_ignores`]).
    pub fn reload_ignores_if_changed(&self, settled: &[PathBuf]) -> Option<Vec<String>> {
        let changed = {
            let current = self.ignores.read().unwrap_or_else(|poisoned| poisoned.into_inner());
            settled.iter().any(|path| {
                path.file_name() == Some(std::ffi::OsStr::new(GITIGNORE_FILE_NAME))
                    || (path.as_path() != self.root
                        && (path.is_dir() || path.is_symlink() || current.links().concerns(path)))
            })
        };
        changed.then(|| self.reload_ignores())
    }

    /// Drops from a settled batch the paths the current layers ignore: an
    /// event that passed [`Self::next_change`] under the old layers but sits
    /// under what a `.gitignore` in the same batch now ignores.
    pub fn retain_unignored(&self, settled: &mut Vec<PathBuf>) {
        let layers = self.ignores.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        settled.retain(|path| !layers.is_ignored(path));
    }

    /// Returns the next change to a non-ignored path, waiting up to
    /// `timeout`, spelled as the walk lists it. `None` means either nothing
    /// arrived in time or the watcher was dropped.
    ///
    /// The link table's remap runs before the ignore check: the real spelling
    /// of a file reached only through a link is gitignored by construction,
    /// so filtering first would drop every event for it. A path under a
    /// refused link is dropped.
    pub fn next_change(&self, timeout: Duration) -> Option<PathBuf> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match self.events.recv_timeout(remaining) {
                Ok(path) => {
                    let layers = self.ignores.read().unwrap_or_else(|poisoned| poisoned.into_inner());
                    let path = match layers.to_indexed(&path) {
                        Remap::Keep => path,
                        Remap::To(indexed) => indexed,
                        Remap::Drop => continue,
                    };
                    if !layers.is_ignored(&path) {
                        return Some(path);
                    }
                }
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

    // -----------------------------------------------------------------
    // Layered, reloadable .gitignore matching
    // -----------------------------------------------------------------

    /// Every change [`ProjectWatcher::next_change`] surfaces until `path` does
    /// (inclusive), or `None` if `path` never surfaced within `timeout`.
    fn changes_until(watcher: &ProjectWatcher, path: &Path, timeout: Duration) -> Option<Vec<PathBuf>> {
        let deadline = std::time::Instant::now() + timeout;
        let mut seen = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            if let Some(changed) = watcher.next_change(remaining) {
                let done = changed == path;
                seen.push(changed);
                if done {
                    return Some(seen);
                }
            }
        }
    }

    /// Every change surfaced within `window`.
    fn changes_within(watcher: &ProjectWatcher, window: Duration) -> Vec<PathBuf> {
        let deadline = std::time::Instant::now() + window;
        let mut seen = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return seen;
            }
            if let Some(changed) = watcher.next_change(remaining) {
                seen.push(changed);
            }
        }
    }

    fn write(root: &Path, relative: &str, contents: &str) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    /// B0: a save under a directory that only a *nested* `.gitignore`
    /// ignores surfaces no change; a sibling outside it still does.
    ///
    /// Control: read only the root `.gitignore` in `IgnoreLayers::load` -
    /// `src/gen/a.ts` surfaces.
    #[test]
    fn a_save_under_a_directory_a_nested_gitignore_ignores_produces_no_event() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write(&root, "src/.gitignore", "gen/\n");
        fs::create_dir_all(root.join("src/gen")).unwrap();

        let watcher = ProjectWatcher::new(&root).unwrap();
        drain_startup_noise(&watcher);

        let ignored_dir = root.join("src/gen");
        write(&root, "src/gen/a.ts", "export const a = 1;\n");
        let tracked = write(&root, "src/b.ts", "export const b = 1;\n");

        let mut seen = changes_until(&watcher, &tracked, EVENT_TIMEOUT)
            .expect("a save outside the nested-ignored directory must still surface");
        // No order is guaranteed between the two writes' events: keep
        // listening after the tracked one.
        seen.extend(changes_within(&watcher, NO_EVENT_TIMEOUT));
        assert!(
            seen.iter().all(|path| !path.starts_with(&ignored_dir)),
            "nothing under src/gen (ignored by src/.gitignore) may surface: {seen:?}"
        );
    }

    /// The deepest layer with an opinion decides: a nested `!` re-includes a
    /// file the root ignores, a nested pattern ignores what the root allows,
    /// and an ignored ancestor directory hides its whole subtree.
    ///
    /// Control: consult the layers root-first in `IgnoreLayers::matched` -
    /// `src/keep.gen.ts` reads as ignored.
    #[test]
    fn the_deepest_gitignore_with_an_opinion_decides() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write(&root, ".gitignore", "*.gen.ts\nbuild/\n");
        write(&root, "src/.gitignore", "!keep.gen.ts\nlocal.ts\n");
        let keep = write(&root, "src/keep.gen.ts", "");
        let other = write(&root, "src/other.gen.ts", "");
        let local = write(&root, "src/local.ts", "");
        let plain = write(&root, "src/plain.ts", "");
        let under_build = write(&root, "build/src/deep.ts", "");

        let watcher = ProjectWatcher::new(&root).unwrap();

        assert!(!watcher.is_ignored(&keep), "src/.gitignore's `!keep.gen.ts` re-includes it");
        assert!(watcher.is_ignored(&other), "the root's *.gen.ts still applies under src");
        assert!(watcher.is_ignored(&local), "src/.gitignore ignores what the root allows");
        assert!(!watcher.is_ignored(&plain));
        assert!(watcher.is_ignored(&under_build), "an ignored ancestor hides its subtree");
    }

    /// An edited `.gitignore` (root or nested) governs matching only after
    /// `reload_ignores`, with no restart; until then the old rules hold.
    ///
    /// Control: make `reload_ignores` a no-op - the un-ignored file still
    /// reads as ignored.
    #[test]
    fn reloading_after_a_gitignore_edit_applies_the_new_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write(&root, ".gitignore", "generated/\n");
        let generated = write(&root, "generated/x.ts", "");
        let nested = write(&root, "src/gen/y.ts", "");

        let watcher = ProjectWatcher::new(&root).unwrap();
        assert!(watcher.is_ignored(&generated));
        assert!(!watcher.is_ignored(&nested));

        write(&root, ".gitignore", "");
        write(&root, "src/.gitignore", "gen/\n");
        assert!(watcher.is_ignored(&generated), "without a reload the layers loaded at start stay");
        assert!(!watcher.is_ignored(&nested), "without a reload a new nested .gitignore is not read");

        watcher.reload_ignores();
        assert!(!watcher.is_ignored(&generated), "the root .gitignore no longer ignores generated/");
        assert!(watcher.is_ignored(&nested), "the new src/.gitignore ignores src/gen/");

        // And the events follow: a save in the un-ignored directory surfaces.
        drain_startup_noise(&watcher);
        write(&root, "generated/x.ts", "export const x = 1;\n");
        assert!(
            changes_until(&watcher, &generated, EVENT_TIMEOUT).is_some(),
            "a save under generated/ must surface once the reloaded layers allow it"
        );
    }

    /// A `.gitignore` that lists itself still surfaces its own edits (or the
    /// layers could never be reloaded from it); one inside an ignored
    /// directory does not.
    ///
    /// Control: drop the `.gitignore` special case in
    /// `IgnoreLayers::is_ignored` - the self-listing file reads as ignored and
    /// its edit never surfaces.
    #[test]
    fn a_gitignore_that_ignores_itself_still_delivers_its_own_event() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let own = write(&root, ".gitignore", ".gitignore\nvendor/\n");
        let in_ignored_dir = write(&root, "vendor/.gitignore", "*.tmp\n");

        let watcher = ProjectWatcher::new(&root).unwrap();
        assert!(!watcher.is_ignored(&own), "a .gitignore listing itself is not ignored");
        assert!(watcher.is_ignored(&in_ignored_dir), "a .gitignore inside an ignored directory is");

        drain_startup_noise(&watcher);
        write(&root, ".gitignore", ".gitignore\nvendor/\n*.log\n");
        assert!(
            changes_until(&watcher, &own, EVENT_TIMEOUT).is_some(),
            "the self-ignoring .gitignore's edit must surface"
        );
    }

    /// `reload_ignores_if_changed` reloads for a settled path named
    /// `.gitignore` (even one already deleted) or a directory other than the
    /// root - a moved-in directory can carry a `.gitignore` no event names -
    /// and not for plain files or the root alone.
    ///
    /// Control: drop the `is_dir` arm in `reload_ignores_if_changed` - the
    /// moved-in directory's rules are never read.
    #[test]
    fn a_settled_directory_or_gitignore_reloads_the_layers_and_plain_files_do_not() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let plain = write(&root, "src/a.ts", "");
        let watcher = ProjectWatcher::new(&root).unwrap();

        // A directory moved in from outside the project, carrying its own
        // .gitignore.
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "pkg/.gitignore", "out/\n");
        write(outside.path(), "pkg/out/built.ts", "");
        fs::rename(outside.path().join("pkg"), root.join("pkg")).unwrap();
        let built = root.join("pkg/out/built.ts");
        assert!(!watcher.is_ignored(&built), "not read before a reload");

        assert!(
            watcher.reload_ignores_if_changed(std::slice::from_ref(&plain)).is_none(),
            "a plain file reloads nothing"
        );
        assert!(
            watcher.reload_ignores_if_changed(std::slice::from_ref(&root)).is_none(),
            "the root alone reloads nothing"
        );
        assert!(!watcher.is_ignored(&built), "still the old layers");

        assert!(watcher.reload_ignores_if_changed(&[plain.clone(), root.join("pkg")]).is_some());
        assert!(watcher.is_ignored(&built), "the moved-in pkg/.gitignore now applies");

        // A deleted .gitignore is still named .gitignore.
        fs::remove_file(root.join("pkg/.gitignore")).unwrap();
        assert!(watcher.reload_ignores_if_changed(&[root.join("pkg/.gitignore")]).is_some());
        assert!(!watcher.is_ignored(&built), "the deleted file's rules are gone");
    }

    /// A path that passed `next_change` under the old layers is dropped from
    /// the batch once a reload (from the same batch's `.gitignore` edit)
    /// ignores it.
    ///
    /// Control: make `retain_unignored` keep everything.
    #[test]
    fn a_batch_is_refiltered_by_the_reloaded_layers() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let generated = write(&root, "generated/x.ts", "");
        let source = write(&root, "src/a.ts", "");
        let watcher = ProjectWatcher::new(&root).unwrap();

        let gitignore = write(&root, ".gitignore", "generated/\n");
        let mut batch = vec![generated.clone(), source.clone(), gitignore.clone()];
        assert!(watcher.reload_ignores_if_changed(&batch).is_some());
        watcher.retain_unignored(&mut batch);
        assert_eq!(batch, vec![source, gitignore]);
    }
}
