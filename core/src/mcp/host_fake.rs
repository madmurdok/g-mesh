//! A test [`McpHost`]: the registry's pure answers over a fixed
//! `DiscoveredPlugins` (no supervisors, no plugin processes, no failed
//! languages), a pending-change counter, a count of recorded requests, and a
//! settable watcher quiet.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use super::host::{Activity, McpHost, PathCoverage};
use crate::plugin_manifest::{Capabilities, DiscoveredPlugins, PluginManifest};
use crate::storage::index_store::IndexStore;

pub(crate) struct FakeHost {
    root: PathBuf,
    discovered: DiscoveredPlugins,
    /// Queued changes `replay_pending` drains.
    pending: AtomicUsize,
    /// How many times `Activity::request` ran.
    requests: AtomicUsize,
    /// What `watcher_quiet` answers; `None` until a test sets it.
    watcher_quiet: Mutex<Option<Duration>>,
}

impl FakeHost {
    /// A host for `root` with no plugins discovered.
    pub(crate) fn new(root: &Path) -> Self {
        Self::over(root, DiscoveredPlugins::default())
    }

    /// A host for `root` with `discovered`'s plugins, none of them failed.
    pub(crate) fn over(root: &Path, discovered: DiscoveredPlugins) -> Self {
        Self {
            root: root.to_path_buf(),
            discovered,
            pending: AtomicUsize::new(0),
            requests: AtomicUsize::new(0),
            watcher_quiet: Mutex::new(None),
        }
    }

    /// Queues `files` changes for the next `replay_pending` to drain.
    pub(crate) fn queue(&self, files: usize) {
        self.pending.fetch_add(files, Ordering::SeqCst);
    }

    /// Makes `watcher_quiet` answer `quiet`, as the daemon would after that
    /// long without a watcher event.
    pub(crate) fn set_watcher_quiet(&self, quiet: Option<Duration>) {
        *self.watcher_quiet.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = quiet;
    }

    /// How many times `Activity::request` has run.
    pub(crate) fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

impl Activity for FakeHost {
    fn request(&self) {
        self.requests.fetch_add(1, Ordering::SeqCst);
    }
}

impl McpHost for FakeHost {
    fn project_root(&self) -> &Path {
        &self.root
    }

    fn manifests(&self) -> Vec<&PluginManifest> {
        self.discovered.manifests.values().collect()
    }

    fn receiver_call_capabilities(&self) -> HashMap<String, Capabilities> {
        self.discovered.manifests.iter().map(|(language, m)| (language.clone(), m.capabilities)).collect()
    }

    fn entry_points(&self) -> Vec<String> {
        let mut points: Vec<String> = self
            .discovered
            .manifests
            .values()
            .flat_map(|m| m.workspace.entry_points.iter().cloned())
            .collect();
        points.sort();
        points.dedup();
        points
    }

    fn missing_languages(&self) -> Vec<&'static str> {
        crate::languages::missing(&self.discovered).into_iter().map(|entry| entry.language).collect()
    }

    fn path_coverage(&self, file_path: &str) -> Option<PathCoverage> {
        if self.discovered.language_for(file_path).is_some() {
            return None;
        }
        crate::languages::absent_for_path(&self.discovered, file_path).map(PathCoverage::Absent)
    }

    fn has_pending(&self) -> bool {
        self.pending.load(Ordering::SeqCst) > 0
    }

    fn pending_summary(&self) -> String {
        format!("fake ({} files)", self.pending.load(Ordering::SeqCst))
    }

    fn replay_pending(&self, _store: &IndexStore) -> usize {
        self.pending.swap(0, Ordering::SeqCst)
    }

    fn ensure_fresh(&self, _store: &IndexStore, _file_path: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    fn watcher_quiet(&self) -> Option<Duration> {
        *self.watcher_quiet.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
