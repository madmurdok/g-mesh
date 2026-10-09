//! A plugin registry over one real bundled plugin (TypeScript by default) and
//! a live index in a temp project, driven the way the daemon's watcher loop
//! drives it. Shared by the tests that route settled paths to that plugin.
//!
//! Needs that plugin's binary built in this profile
//! (`cargo build --workspace`).

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use g_mesh::daemon::manifest;
use g_mesh::daemon::registry::PluginRegistry;
use g_mesh::embedding::EmbeddingPipeline;
use g_mesh::storage::connection::{self, project_dir};
use g_mesh::storage::index_store::IndexStore;
use g_mesh::storage::schema;

pub struct Harness {
    project: tempfile::TempDir,
    _plugins: tempfile::TempDir,
    pub registry: PluginRegistry,
    pub conn: IndexStore,
}

impl Harness {
    /// The discovery root links only the checked-in TypeScript plugin
    /// directory, so its manifest and `${G_MESH_BIN_DIR}` command resolve as
    /// they do from the checkout. Nothing is spawned yet.
    pub fn new() -> Self {
        Self::with_plugin("typescript")
    }

    /// The same harness over another checked-in plugin directory
    /// (`plugins/<language>`).
    pub fn with_plugin(language: &str) -> Self {
        let plugins = tempfile::tempdir().expect("failed to create a plugin root");
        let checkout = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins").join(language);
        std::os::unix::fs::symlink(
            checkout.canonicalize().expect("the checked-in plugin directory"),
            plugins.path().join(language),
        )
        .expect("failed to link the plugin directory");
        Self::over(plugins)
    }

    /// The harness over a copy of the checked-in manifest of `language`,
    /// rewritten by `edit`. Its `${G_MESH_BIN_DIR}` command still names the
    /// plugin binary built in this profile.
    pub fn with_manifest(language: &str, edit: impl FnOnce(String) -> String) -> Self {
        let plugins = tempfile::tempdir().expect("failed to create a plugin root");
        let checkout = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins").join(language);
        let text = std::fs::read_to_string(checkout.join("plugin.toml")).expect("the checked-in manifest");
        let dir = plugins.path().join(language);
        std::fs::create_dir_all(&dir).expect("failed to create the plugin directory");
        std::fs::write(dir.join("plugin.toml"), edit(text)).expect("failed to write the manifest");
        Self::over(plugins)
    }

    fn over(plugins: tempfile::TempDir) -> Self {
        let project = tempfile::tempdir().expect("failed to create a project root");
        let discovered =
            manifest::discover(&[plugins.path().to_path_buf()]).expect("the plugin manifest discovers");
        let root = project.path().canonicalize().expect("failed to canonicalize the project root");
        let state_dir = project_dir(&root).expect("failed to resolve the state directory");
        std::fs::create_dir_all(&state_dir).expect("failed to create the state directory");
        let registry = PluginRegistry::new(
            &root,
            state_dir,
            discovered,
            None,
            None,
            Arc::new(EmbeddingPipeline::disabled()),
        );
        // File-backed, where the daemon keeps it: a workspace reindex attaches
        // the live index by its path.
        let live = connection::open(&root).expect("failed to open the live index");
        schema::ensure_current(&live, "test-generation").expect("failed to apply the schema");
        Self { project, _plugins: plugins, registry, conn: IndexStore::new(live) }
    }

    pub fn root(&self) -> PathBuf {
        self.project.path().canonicalize().unwrap()
    }

    pub fn write(&self, rel: &str, contents: &str) {
        let path = self.root().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    /// Routes one settled path, as the watcher loop does for each path of a
    /// batch. Returns once the plugin's answer is applied.
    pub fn route(&self, file_path: &str) {
        self.registry.route_settled_path(&self.conn, file_path.to_string());
    }

    /// `file:name` of every node `from`'s file node reaches by an `IMPORTS`
    /// edge whose `resolved` flag is `resolved`, sorted.
    pub fn imports_of(&self, from: &str, resolved: bool) -> Vec<String> {
        self.conn.with(|c| {
            let mut statement = c
                .prepare(
                    "SELECT t.filePath || ':' || t.name FROM edges e \
                     JOIN nodes f ON f.id = e.fromId JOIN nodes t ON t.id = e.toId \
                     WHERE e.kind = 'IMPORTS' AND f.filePath = ?1 AND e.resolved = ?2 \
                     ORDER BY 1",
                )
                .unwrap();
            statement
                .query_map(rusqlite::params![from, resolved as i64], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        })
    }

    /// `(resolved, unresolved)` imports of `from`.
    pub fn imports(&self, from: &str) -> (Vec<String>, Vec<String>) {
        (self.imports_of(from, true), self.imports_of(from, false))
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.registry.sleep_all_now("test finished");
        if let Ok(state) = project_dir(self.project.path()) {
            let _ = std::fs::remove_dir_all(state);
        }
    }
}
