//! What the MCP server needs from the process hosting it, as traits `mcp`
//! owns: the daemon implements them (its `mcp_host` module), a test passes a
//! fake. The server also holds the store, the embedding pipeline and an
//! [`IndexingStatus`](crate::indexing_status::IndexingStatus) directly; the
//! status is the host's other half, kept out of the trait so its one async
//! method (`wait_for`) needs no boxed future.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use crate::plugin_manifest::{Capabilities, PluginManifest};
use crate::storage::index_store::IndexStore;

/// Records that the core did something a user asked for (the idle clock).
pub trait Activity: Send + Sync {
    fn request(&self);
}

/// Everything the MCP server needs from the process hosting it, besides the
/// store, the embedding pipeline and `IndexingStatus`, which it holds directly.
///
/// # Lock order
///
/// `replay_pending` and `ensure_fresh` take plugin-side locks (registry
/// supervisors, supervisor state, plugin process state) and the store only
/// inside them (`storage::index_store`'s module doc). So a caller:
/// - holds no store guard when it calls either of them;
/// - records use (`Activity::request` and the `lastUsed` store write) before
///   the replay, never from inside it;
/// - calls them off the async workers (`spawn_blocking`): they may spawn a
///   plugin process.
///
/// Every other method takes no store lock and does no I/O except
/// `path_coverage` (one `canonicalize`).
pub trait McpHost: Activity + 'static {
    /// The canonicalized project root.
    fn project_root(&self) -> &Path;
    /// Every discovered plugin's manifest, fixed for the host's lifetime.
    fn manifests(&self) -> Vec<&PluginManifest>;
    /// Each discovered language's receiver-call capabilities.
    fn receiver_call_capabilities(&self) -> HashMap<String, Capabilities>;
    /// The project's entry-point paths, as the plugins declare them.
    fn entry_points(&self) -> Vec<String>;
    /// Catalogued languages present in the project with no plugin installed.
    fn missing_languages(&self) -> Vec<&'static str>;
    /// Why the index can hold nothing of `file_path`; `None` when nothing
    /// explains an empty answer.
    fn path_coverage(&self, file_path: &str) -> Option<PathCoverage>;
    /// Whether changes queued while a plugin slept wait to be replayed.
    fn has_pending(&self) -> bool;
    /// A short description of the queued changes, for a progress message.
    fn pending_summary(&self) -> String;
    /// Replays the queued changes into `store`; returns how many were applied.
    fn replay_pending(&self, store: &IndexStore) -> usize;
    /// `Ok(None)`: no plugin claims the file. `Ok(Some(label))`: the check's
    /// outcome, as the trace line prints it.
    fn ensure_fresh(&self, store: &IndexStore, file_path: &str) -> anyhow::Result<Option<String>>;
    /// How long the file watcher has delivered no event, when that is long
    /// enough for a by-name answer to say so; `None` while events are recent
    /// or no watcher listens.
    fn watcher_quiet(&self) -> Option<Duration>;
}

/// Why a path-anchored answer about one file can be empty while the file
/// exists: [`McpHost::path_coverage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathCoverage {
    /// No discovered plugin claims the file; the catalogue entry of the
    /// plugin that would index it.
    Absent(&'static crate::languages::CatalogueEntry),
    /// The discovered plugin for this language failed its bulk walk, so
    /// nothing of the language is in the index (ADR 0021, section 2).
    Failed(String),
}
