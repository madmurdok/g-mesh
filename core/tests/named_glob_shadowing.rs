//! A production composition root links with the discovered plugins'
//! `[plugin.reexports]` rules (docs/adr/0020-named-reexport-shadows-glob.md).
//!
//! `g-mesh init`, the real binary, discovers the real, checked-in
//! `plugins/rust/plugin.toml` (alone, through the roots override) and builds
//! the index with the real Rust plugin. The fixture: a parent's
//! explicit `use std::fmt::Error;` beside its `use self::x::*;`, where `x`
//! declares its own `Error::load`, and a test module reaching both through
//! `use super::*`. Rust's explicit `use` shadows the glob, so the call must not
//! land on `x::Error::load`. A store built without the manifests' rules links
//! with no shadowing, and the glob then hands the call `x::Error::load`: the
//! wrong edge. The same project without the explicit `use`
//! shows the glob does link `x::Error::load` through this root.
//!
//! The external `use` is chosen over a project one on purpose: `init` ends
//! with rust-analyzer's semantic pass, which would repair an unresolved call
//! to a project item and hide a missing rule, but has no project item to give
//! `std::fmt::Error::load`.
//!
//! Control: in `cli::init::init`, build the walk's store with
//! `IndexStore::new(conn)` alone (no `with_link_rules`) - the shadowed call
//! links `x::Error::load` and this fails.

use std::path::{Path, PathBuf};
use std::process::Command;

use g_mesh::storage::connection::project_dir;
use rusqlite::Connection;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// `src/user.rs`: the named `use` line `named`, then `use self::x::*;`, `x`'s
/// `Error::load`, and a test module calling `Error::load` through
/// `use super::*`.
fn user_rs(named: &str) -> String {
    format!(
        "{named}\nuse self::x::*;\n\npub mod x {{\n    pub struct Error;\n\n    impl Error {{\n        \
         pub fn load(c: u32) {{\n            let _ = c;\n        }}\n    }}\n}}\n\n#[cfg(test)]\nmod tests {{\n    \
         use super::*;\n\n    #[test]\n    fn loads() {{\n        Error::load(1);\n    }}\n}}\n"
    )
}

struct Project {
    dir: tempfile::TempDir,
    plugins: tempfile::TempDir,
}

impl Project {
    fn new(user: &str) -> Self {
        let project = Self {
            dir: tempfile::tempdir().expect("failed to create a temp project root"),
            plugins: tempfile::tempdir().expect("failed to create a temp plugin root"),
        };
        let files = [
            ("Cargo.toml", "[package]\nname = \"krate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
            ("src/lib.rs", "pub mod user;\n"),
            ("src/user.rs", user),
        ];
        for (path, contents) in files {
            let full = project.root().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).expect("failed to create a fixture directory");
            std::fs::write(&full, contents).expect("failed to write the fixture");
        }
        // Discovery sees only the real Rust manifest, copied from its
        // checked-in file: its `${G_MESH_BIN_DIR}` still resolves to this
        // profile's `target/` directory (`daemon_plugin_bin_dir.rs`).
        let rust = project.plugins.path().join("rust");
        std::fs::create_dir_all(&rust).expect("failed to create the plugin directory");
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/rust/plugin.toml");
        std::fs::copy(manifest, rust.join("plugin.toml")).expect("failed to copy the Rust manifest");
        project
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn index_path(&self) -> PathBuf {
        project_dir(self.root()).expect("failed to resolve the project state directory").join("index.db")
    }

    /// Runs `g-mesh init` and returns `(target qualifiedName, resolved)` of
    /// every `CALLS` edge out of `loads`, checking the walk indexed
    /// `x::Error::load` and the call.
    fn init_and_read_calls(&self) -> Vec<(String, bool)> {
        let output = Command::new(BIN)
            .arg("init")
            .current_dir(self.root())
            .env("G_MESH_PLUGIN_ROOTS_OVERRIDE", self.plugins.path())
            .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir")
            .output()
            .expect("failed to run `g-mesh init`");
        assert!(
            output.status.success(),
            "`g-mesh init` failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let conn = Connection::open(self.index_path()).expect("failed to open the index");
        let declared: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE filePath = 'src/user.rs' AND name = 'load' AND kind = 'Function'",
                [],
                |row| row.get(0),
            )
            .expect("failed to count the declarations");
        assert_eq!(declared, 1, "the walk must index `x::Error::load`");

        let mut stmt = conn
            .prepare(
                "SELECT t.qualifiedName, e.resolved FROM edges e \
                 JOIN nodes f ON f.id = e.fromId JOIN nodes t ON t.id = e.toId \
                 WHERE f.name = 'loads' AND e.kind = 'CALLS' ORDER BY t.qualifiedName",
            )
            .expect("failed to prepare the edge query");
        let calls: Vec<(String, bool)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("failed to read the edges")
            .collect::<rusqlite::Result<_>>()
            .expect("failed to collect the edges");
        assert!(!calls.is_empty(), "the walk must record the test module's call");
        calls
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

fn links_x_load(calls: &[(String, bool)]) -> bool {
    calls.iter().any(|(target, resolved)| *resolved && target.ends_with("x::Error::load"))
}

#[test]
fn init_links_rust_with_its_manifests_glob_shadowing() {
    let shadowed = Project::new(&user_rs("use std::fmt::Error;"));
    let calls = shadowed.init_and_read_calls();
    assert!(!links_x_load(&calls), "the explicit `use` must shadow the glob: {calls:?}");

    let glob_only = Project::new(&user_rs(""));
    let calls = glob_only.init_and_read_calls();
    assert!(links_x_load(&calls), "without the explicit `use` the glob links `x::Error::load`: {calls:?}");
}
