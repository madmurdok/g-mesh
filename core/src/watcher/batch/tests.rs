use super::*;
use crate::daemon::test_plugin;
use crate::storage::write::upsert_indexed_file;
use std::fs;

use SettledKind::{Created, Deleted, Modified};

/// An index with an `indexed_files` baseline for each of `baselines`.
fn index_with_baselines(baselines: &[&str]) -> IndexStore {
    let store = test_plugin::empty_index();
    for file_path in baselines {
        store.with(|c| upsert_indexed_file(c, file_path, 1, "hash")).unwrap();
    }
    store
}

#[test]
fn a_batch_routes_deletions_then_creations_then_modifications_whatever_its_input_order() {
    let batch = vec![
        (Modified, "importer"),
        (Created, "new-target"),
        (Deleted, "gone"),
        (Modified, "other-importer"),
        (Created, "other-new-target"),
        (Deleted, "other-gone"),
    ];

    assert_eq!(
        order_for_routing(batch),
        vec!["gone", "other-gone", "new-target", "other-new-target", "importer", "other-importer"],
        "deletions, then creations, then modifications"
    );
}

#[test]
fn paths_of_the_same_kind_keep_the_order_the_batch_had() {
    // Names sort the opposite way to the input, so an order that fell back
    // to the item (or reversed equal keys) would show.
    let batch = vec![
        (Modified, "m3"),
        (Created, "c3"),
        (Modified, "m2"),
        (Created, "c2"),
        (Modified, "m1"),
        (Created, "c1"),
        (Deleted, "d3"),
        (Deleted, "d2"),
        (Deleted, "d1"),
    ];

    assert_eq!(
        order_for_routing(batch),
        vec!["d3", "d2", "d1", "c3", "c2", "c1", "m3", "m2", "m1"],
        "routing order must be stable within each kind"
    );
}

#[test]
fn ordering_neither_drops_nor_duplicates_a_path() {
    let batch: Vec<(SettledKind, usize)> =
        (0..30).map(|i| ([Modified, Created, Deleted][i % 3], i)).collect();

    let mut routed = order_for_routing(batch);
    assert_eq!(routed.len(), 30, "each path is routed exactly once");
    routed.sort_unstable();
    assert_eq!(routed, (0..30).collect::<Vec<_>>());
}

#[test]
fn a_path_absent_on_disk_is_a_deletion_even_with_a_baseline() {
    let root = tempfile::tempdir().unwrap();
    let store = index_with_baselines(&["gone.rs"]);

    assert_eq!(classify_settled(&store, &root.path().join("gone.rs"), "gone.rs"), Deleted);
    assert_eq!(
        classify_settled(&store, &root.path().join("never.rs"), "never.rs"),
        Deleted,
        "absent and never indexed is still a deletion"
    );
}

#[test]
fn a_path_on_disk_without_a_baseline_is_a_creation() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("new.rs"), "").unwrap();
    let store = index_with_baselines(&["other.rs"]);

    assert_eq!(classify_settled(&store, &root.path().join("new.rs"), "new.rs"), Created);
}

#[test]
fn a_path_on_disk_with_a_baseline_is_a_modification() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("known.rs"), "").unwrap();
    let store = index_with_baselines(&["known.rs"]);

    assert_eq!(classify_settled(&store, &root.path().join("known.rs"), "known.rs"), Modified);
}

#[test]
fn has_indexed_baseline_reads_the_presence_of_an_indexed_files_row() {
    let store = index_with_baselines(&["known.rs"]);

    assert!(store.with(|c| has_indexed_baseline(c, "known.rs")).unwrap());
    assert!(!store.with(|c| has_indexed_baseline(c, "unknown.rs")).unwrap());
}
