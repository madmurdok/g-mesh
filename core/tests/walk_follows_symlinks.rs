//! Acceptance for ADR 0025 (B13 in
//! `docs/architecture/gm-349-sdk-walk-symlinks.md`, section 7): a real
//! `g-mesh init` over a project whose generated sources live in a gitignored
//! `gen/`, reached only through the link `src/gen -> ../gen`, indexes every
//! language's file under `src/gen/...` and nothing under `gen/...`.
//!
//! Real plugin binaries: Rust and Python (the SDK walk) and Go (its own
//! walk), discovered through links to their checked-in plugin directories.
//! Like the rest of the suite's Go tests it needs the Go plugin binary
//! `core/build.rs` builds, and the Rust and Python plugin binaries built in
//! this profile (`cargo build --workspace`). TypeScript has no arm here: its
//! plugin does not run the SDK walk.
//!
//! Controls: `follow_links(false)` in the SDK's `walker`
//! (`plugins/sdk/src/walk.rs`) -> the Rust and Python rows are missing; in
//! the Go plugin's `walkDir`, run the symlink guard before the `.gitignore`
//! check (or never follow a link) -> the Go row is missing.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use g_mesh::daemon;
use g_mesh::daemon::bulk_index::ABSENT_COUNT_OFF_ENV;
use g_mesh::storage::connection::project_dir;
use rusqlite::Connection;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const NO_MODEL_DIR: &str = "/nonexistent-g-mesh-test-model-dir";

/// The generated files, one per language, each declaring one symbol.
const GENERATED: [(&str, &str, &str, &str); 3] = [
    ("rust", "gen_rs.rs", "generated_rust_symbol", "pub fn generated_rust_symbol() -> u32 {\n    1\n}\n"),
    ("python", "gen_py.py", "generated_python_symbol", "def generated_python_symbol():\n    pass\n"),
    ("go", "gen_go.go", "GeneratedGoSymbol", "package gen\n\nfunc GeneratedGoSymbol() int { return 1 }\n"),
];

struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let project = Self { dir: tempfile::tempdir().expect("failed to create a temp project root") };
        let mut files = vec![
            (
                "Cargo.toml".to_string(),
                "[package]\nname = \"krate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("src/lib.rs".to_string(), "pub fn plain_rust_symbol() -> u32 {\n    1\n}\n"),
            ("go.mod".to_string(), "module fixture\n\ngo 1.21\n"),
            // Anchored: a bare `gen/` would ignore the link `src/gen` too.
            (".gitignore".to_string(), "/gen/\n"),
        ];
        for (_, file, _, text) in GENERATED {
            files.push((format!("gen/{file}"), text));
        }
        for (rel, contents) in files {
            let path = project.root().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).expect("failed to create a fixture directory");
            std::fs::write(path, contents).expect("failed to write a fixture file");
        }
        std::os::unix::fs::symlink("../gen", project.root().join("src/gen")).expect("failed to link src/gen");
        project
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn init(&self, plugins: &Path) -> Output {
        Command::new(BIN)
            .arg("init")
            .current_dir(self.root())
            .env("G_MESH_PLUGIN_ROOTS_OVERRIDE", plugins)
            .env(g_mesh::embedding::model::MODEL_DIR_ENV, NO_MODEL_DIR)
            .env_remove(ABSENT_COUNT_OFF_ENV)
            .output()
            .expect("failed to run `g-mesh init`")
    }

    /// The `filePath` of every node named `name` in `language`, sorted.
    fn paths_of(&self, language: &str, name: &str) -> Vec<String> {
        let path = project_dir(self.root()).expect("failed to resolve the state directory").join("index.db");
        let conn = Connection::open(path).expect("failed to open the index");
        let mut statement = conn
            .prepare("SELECT filePath FROM nodes WHERE language = ?1 AND name = ?2 ORDER BY filePath")
            .expect("failed to prepare");
        let rows = statement
            .query_map([language, name], |row| row.get::<_, String>(0))
            .expect("failed to query")
            .collect::<Result<Vec<_>, _>>()
            .expect("failed to read a row");
        rows
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if let Ok(path) = daemon::pid_path(self.root()) {
            common::kill_pid_file(&path);
        }
        if let Ok(endpoint) = daemon::endpoint(self.root()) {
            endpoint.clear_stale();
        }
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

/// A discovery root holding links to the checked-in `rust`, `python` and
/// `go` plugin directories, so each manifest's relative and
/// `${G_MESH_BIN_DIR}` commands resolve as they do from the checkout.
fn rust_python_go_plugin_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("failed to create a plugin discovery root");
    let checkout = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins");
    for (language, _, _, _) in GENERATED {
        let dir = checkout.join(language).canonicalize().expect("a checked-in plugin directory");
        std::os::unix::fs::symlink(dir, root.path().join(language))
            .expect("failed to link a plugin directory");
    }
    root
}

/// B13: every language's file in the gitignored `gen/` is indexed under
/// the link's spelling `src/gen/...`, once, and the plain crate beside it
/// still is.
#[test]
fn init_indexes_a_gitignored_target_reached_through_a_link_under_the_link_spelling() {
    let project = Project::new();
    let plugins = rust_python_go_plugin_root();

    let output = project.init(plugins.path());

    assert!(
        output.status.success(),
        "init: status {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(project.paths_of("rust", "plain_rust_symbol"), vec!["src/lib.rs"]);
    for (language, file, symbol, _) in GENERATED {
        assert_eq!(
            project.paths_of(language, symbol),
            vec![format!("src/gen/{file}")],
            "{language}: indexed once, under the link's spelling"
        );
    }
}
