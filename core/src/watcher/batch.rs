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

/// Puts a classified batch into routing order: deletions, then creations,
/// then modifications, stable within each kind.
pub fn order_for_routing<T>(mut batch: Vec<(SettledKind, T)>) -> Vec<T> {
    batch.sort_by_key(|(kind, _)| *kind);
    batch.into_iter().map(|(_, item)| item).collect()
}
