//! A Python module's presence changing mid-session moves what an importer
//! extracted afterwards resolves to, through core's registry and the real
//! Python plugin, with no workspace reload: a created module resolves, a
//! deleted one stops resolving, and a module created in the same watcher
//! batch as its importer, routed importer first, resolves on the importer's
//! first extraction (docs/adr/0023-project-model-tracks-file-presence.md,
//! docs/adr/0026-batch-created-files-notification.md).
//!
//! The plugin answers "is `pkg.mod` one of ours" from the module set its
//! project model loaded at spawn, so each test spawns the plugin on a seed
//! file before touching the module: a process spawned later would read the
//! disk and resolve the import anyway. An import of one of ours is an
//! `IMPORTS` edge onto that module's container node; an import of anything
//! else lands on an unresolved `external_module` node. Only syntactic edges
//! are read: the semantic tier (pyright, when it is installed) answers on its
//! own schedule and is not what these tests are about.
//!
//! Controls are listed on each test.

#![cfg(unix)]

mod typescript_registry;
use typescript_registry::Harness;

/// `(target name, target nativeKind, resolved)` of every syntactic `IMPORTS`
/// edge out of `from`'s nodes, sorted.
fn imports(harness: &Harness, from: &str) -> Vec<(String, String, bool)> {
    harness.conn.with(|c| {
        let mut statement = c
            .prepare(
                "SELECT t.name, ifnull(t.nativeKind, ''), e.resolved FROM edges e \
                 JOIN nodes f ON f.id = e.fromId JOIN nodes t ON t.id = e.toId \
                 WHERE e.kind = 'IMPORTS' AND e.source = 'syntactic' AND f.filePath = ?1 \
                 ORDER BY 1, 2, 3",
            )
            .unwrap();
        statement
            .query_map([from], |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? != 0)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    })
}

fn ours(key: &str) -> (String, String, bool) {
    (key.to_string(), "container".to_string(), true)
}

fn external(key: &str) -> (String, String, bool) {
    (key.to_string(), "external_module".to_string(), false)
}

/// The plugin running, its project model loaded over `seed.py` and whatever
/// was written before.
fn spawned(harness: &Harness) {
    harness.write("seed.py", "SEED = 1\n");
    harness.route("seed.py");
}

/// Control: `PythonExtractor::file_presence_changed` in
/// `plugins/python/src/extractor/mod.rs` doing nothing (or deleted, back to
/// the SDK default) -> `pkg.new` is an `external_module`.
#[test]
fn a_module_created_after_spawn_resolves_for_an_importer_extracted_afterwards() {
    let harness = Harness::with_plugin("python");
    spawned(&harness);

    harness.write("pkg/new.py", "def new():\n    return 1\n");
    harness.route("pkg/new.py");
    harness.write("main.py", "import pkg.new\n");
    harness.route("main.py");

    assert_eq!(imports(&harness, "main.py"), vec![ours("pkg.new")], "pkg.new is one of ours, with no reload");
}

/// Control: `ProjectContext::remove` in `plugins/python/src/project/mod.rs`
/// not calling `unregister_container` (or the extractor's
/// `file_presence_changed` doing nothing) -> `pkg.old` is still answered as
/// one of ours and is not an `external_module`.
#[test]
fn a_module_deleted_after_spawn_stops_resolving_for_an_importer_extracted_afterwards() {
    let harness = Harness::with_plugin("python");
    harness.write("pkg/old.py", "def old():\n    return 1\n");
    spawned(&harness);
    // Indexed, as the bulk walk would have, so its container node exists.
    harness.route("pkg/old.py");
    harness.write("before.py", "import pkg.old\n");
    harness.route("before.py");
    assert_eq!(imports(&harness, "before.py"), vec![ours("pkg.old")], "the loaded module resolves first");

    std::fs::remove_file(harness.root().join("pkg/old.py")).unwrap();
    harness.route("pkg/old.py");
    harness.write("after.py", "import pkg.old\n");
    harness.route("after.py");

    assert_eq!(imports(&harness, "after.py"), vec![external("pkg.old")], "pkg.old is no longer one of ours");
}

/// Core guarantees no order among one batch's creations, so this fixes the
/// worst one, importer first, and drives the registry as the watcher loop
/// does: announce the batch's created paths, then route each.
///
/// Control: `files_created = false` in `plugins/python/plugin.toml` (or the
/// extractor's `file_presence_changed` doing nothing) -> `pkg.new` is an
/// `external_module` on the importer's first extraction.
#[test]
fn an_importer_created_with_its_module_and_routed_first_resolves_the_import() {
    let harness = Harness::with_plugin("python");
    spawned(&harness);

    harness.write("pkg/new.py", "def new():\n    return 1\n");
    harness.write("main.py", "import pkg.new\n");
    let created = ["main.py".to_string(), "pkg/new.py".to_string()];
    harness.registry.announce_created(&created);
    for file_path in &created {
        harness.route(file_path);
    }

    assert_eq!(imports(&harness, "main.py"), vec![ours("pkg.new")], "linked on main.py's first extraction");
}
