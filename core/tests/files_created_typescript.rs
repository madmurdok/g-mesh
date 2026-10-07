//! A watcher batch that creates an importer and its target together, routed
//! importer first, still resolves the import, through the real TypeScript
//! plugin (docs/adr/0026-batch-created-files-notification.md).
//!
//! The plugin resolves an import against the set of files its project model
//! holds, loaded once at spawn. A file created afterwards enters the set only
//! through a presence update, so the importer resolves only if the plugin
//! heard of the target before extracting the importer. Core guarantees no
//! order among one batch's creations (the debouncer drains a hash map), so
//! the test fixes the worst one, importer first, and drives the registry as
//! the watcher loop does: announce the batch's created paths, then route
//! each.
//!
//! Controls: `files_created = false` in `plugins/typescript/plugin.toml`, or a
//! `file_presence_changed` in the plugin's extractor that does nothing ->
//! the import stays unresolved.

#![cfg(unix)]

mod typescript_registry;
use typescript_registry::Harness;

#[test]
fn an_importer_created_with_its_target_and_routed_first_resolves_the_import() {
    let harness = Harness::new();
    // The plugin must be running before the batch: a process spawned by the
    // batch would load both files from disk and resolve the import anyway.
    harness.write("seed.ts", "export const seed = 1;\n");
    harness.route("seed.ts");

    harness.write("b.ts", "export function target(): number {\n  return 1;\n}\n");
    harness.write("a.ts", "import { target } from \"./b\";\n\nexport const value = target();\n");
    let created = ["a.ts".to_string(), "b.ts".to_string()];
    harness.registry.announce_created(&created);
    for file_path in &created {
        harness.route(file_path);
    }

    assert_eq!(
        harness.imports("a.ts"),
        (vec!["b.ts:b.ts".to_string()], Vec::new()),
        "a.ts's import of ./b resolves to b.ts's file node, and nothing stays unresolved"
    );
}
