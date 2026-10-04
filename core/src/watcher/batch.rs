//! Routing order inside one drained debounce batch.
//!
//! Invariant: within a batch, every deletion is routed before every creation,
//! and every creation before every modification; paths of the same kind keep
//! the order the batch had. A plugin's project model learns a file's presence
//! from that file's own `fileChanged` (ADR 0023), so this order is what lets a
//! modified importer be extracted against a model that has already dropped
//! every file the batch deleted and learnt every file it created. The order is
//! per batch only: nothing is held back for a later batch and nothing is
//! routed twice.
//!
//! Invariant: a batch's created paths are announced together, per language,
//! after its deletions are routed and before the first of them is routed
//! (`PluginRegistry::announce_created`), so two files created in one batch
//! are both present in the plugin's model when either is extracted. The
//! announcement adds no routing: each created path is still routed once.
//!
//! See docs/adr/0023-project-model-tracks-file-presence.md (window W1).

use std::path::Path;

use crate::storage::index_store::IndexStore;
use crate::watcher::staleness::has_indexed_baseline;

/// What a settled path is, as far as the routing order needs to know.
/// Declaration order is routing order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SettledKind {
    /// Not on disk when the batch was drained.
    Deleted,
    /// On disk, and the index holds no baseline for it.
    Created,
    /// On disk, and the index holds a baseline for it.
    Modified,
}

/// Classifies one settled path from its state when the batch is drained,
/// never from the raw event kinds: the debouncer coalesces them, and macOS
/// FSEvents already merges create/modify flags. Absent on disk is a deletion;
/// present without an `indexed_files` baseline is a creation; anything else a
/// modification. A known file misread as a creation (its baseline was never
/// recorded) is only routed earlier than needed; a lookup error reads as a
/// modification, today's position.
pub fn classify_settled(conn: &IndexStore, absolute: &Path, file_path: &str) -> SettledKind {
    if !absolute.exists() {
        return SettledKind::Deleted;
    }
    match conn.with(|c| has_indexed_baseline(c, file_path)) {
        Ok(false) => SettledKind::Created,
        Ok(true) | Err(_) => SettledKind::Modified,
    }
}

/// A classified batch split into its routing runs, each in the order the
/// batch had. Routed as `deleted`, then `created`, then `modified`; the split
/// lets the caller announce `created` between the first two runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingOrder<T> {
    pub deleted: Vec<T>,
    pub created: Vec<T>,
    pub modified: Vec<T>,
}

impl<T> RoutingOrder<T> {
    /// Every path in routing order: deletions, creations, modifications.
    pub fn into_routed(self) -> Vec<T> {
        let mut routed = self.deleted;
        routed.extend(self.created);
        routed.extend(self.modified);
        routed
    }
}

/// Puts a classified batch into routing order: deletions, then creations,
/// then modifications, stable within each kind.
pub fn order_for_routing<T>(batch: Vec<(SettledKind, T)>) -> RoutingOrder<T> {
    let mut order = RoutingOrder { deleted: Vec::new(), created: Vec::new(), modified: Vec::new() };
    for (kind, item) in batch {
        match kind {
            SettledKind::Deleted => order.deleted.push(item),
            SettledKind::Created => order.created.push(item),
            SettledKind::Modified => order.modified.push(item),
        }
    }
    order
}

#[cfg(test)]
mod tests;
