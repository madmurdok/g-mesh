//! The daemon's side of `mcp::McpHost`: the plugin registry and the idle
//! clock, behind the traits the MCP server owns, so `mcp` names no `daemon`
//! type.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::lifecycle::CoreActivity;
use super::registry::PluginRegistry;
use crate::mcp::{Activity, McpHost, PathCoverage};
use crate::plugin_manifest::{Capabilities, PluginManifest};
use crate::storage::index_store::IndexStore;

/// One per daemon, shared by every MCP connection: delegates each
/// [`McpHost`] method to the registry and [`Activity::request`] to the
/// core's idle clock.
pub(crate) struct DaemonHost {
    registry: Arc<PluginRegistry>,
    activity: Arc<CoreActivity>,
}

impl DaemonHost {
    pub(crate) fn new(registry: Arc<PluginRegistry>, activity: Arc<CoreActivity>) -> Self {
        Self { registry, activity }
    }
}

impl Activity for CoreActivity {
    fn request(&self) {
        // The inherent method, named by path: a bare `self.request()` here
        // would pick the same inherent method, but the path makes it plain
        // that this is no recursion.
        CoreActivity::request(self)
    }
}

impl Activity for DaemonHost {
    fn request(&self) {
        CoreActivity::request(&self.activity)
    }
}

impl McpHost for DaemonHost {
    fn project_root(&self) -> &Path {
        self.registry.project_root()
    }

    fn manifests(&self) -> Vec<&PluginManifest> {
        self.registry.manifests().collect()
    }

    fn receiver_call_capabilities(&self) -> HashMap<String, Capabilities> {
        self.registry.receiver_call_capabilities()
    }

    fn entry_points(&self) -> Vec<String> {
        self.registry.entry_points()
    }

    fn missing_languages(&self) -> Vec<&'static str> {
        self.registry.missing_languages()
    }

    fn path_coverage(&self, file_path: &str) -> Option<PathCoverage> {
        self.registry.path_coverage(file_path)
    }

    fn has_pending(&self) -> bool {
        self.registry.has_pending()
    }

    fn pending_summary(&self) -> String {
        self.registry.pending_summary()
    }

    fn replay_pending(&self, store: &IndexStore) -> usize {
        self.registry.replay_pending(store)
    }

    fn ensure_fresh(&self, store: &IndexStore, file_path: &str) -> anyhow::Result<Option<String>> {
        // The outcome's `Debug` label is all `mcp` does with it (its trace
        // line); returning `StalenessOutcome` would add `mcp -> watcher`.
        self.registry
            .ensure_fresh(store, file_path)
            .map(|outcome| outcome.map(|outcome| format!("{outcome:?}")))
    }

    fn watcher_quiet(&self) -> Option<Duration> {
        self.registry.watcher_clock().quiet_worth_saying()
    }
}
