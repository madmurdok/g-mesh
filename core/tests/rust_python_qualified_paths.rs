//! The Rust and Python plugins' `qualifiedPath`s, end to end: a fixture
//! extracted by the real plugin binaries, sent over the real NDJSON wire and
//! committed by the real write path, then looked up by partial path through
//! `graph::queries::find_by_qualified_suffix`.
//!
//! Every declaration of the fixture must arrive with its path stored (core
//! drops an invalid path and stores NULL), and a lookup must find exactly the
//! declaration the spelling names: a trait-impl method by its `X::m` alias, a
//! field by `T.f` and never the same-named method `T::f`, and the reverse.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use g_mesh::daemon::bulk_index;
use g_mesh::daemon::manifest::{read_manifest, DiscoveredPlugins};
use g_mesh::graph::queries::find_by_qualified_suffix;
use g_mesh::storage::connection::{open, project_dir};
use g_mesh::storage::index_store::IndexStore;
use g_mesh::storage::schema;
use rusqlite::Connection;

const LIB_RS: &str = "pub mod store;\n";

const STORE_RS: &str = r#"
pub trait Show { fn show(&self) -> u8; }
pub struct Holder<T> { pub inner: T }
impl<T> Holder<T> {
    pub fn inner(&self) -> u8 { 0 }
}
impl<'a, T> Show for &'a Holder<T> {
    fn show(&self) -> u8 { 1 }
}
"#;

const SHAPES_PY: &str =
    "class Outer:\n    class Inner:\n        def m(self):\n            pass\n\ndef top():\n    pass\n";

/// The Rust and Python plugins only, read from their checked-in manifests,
/// whose `${G_MESH_BIN_DIR}` resolves to this profile's `target/` directory.
fn rust_and_python() -> DiscoveredPlugins {
    let plugins = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins");
    let mut manifests = HashMap::new();
    for language in ["rust", "python"] {
        let manifest = read_manifest(&plugins.join(language)).expect("a checked-in plugin manifest");
        manifests.insert(language.to_string(), manifest);
    }
    let routing =
        HashMap::from([(".rs".to_string(), "rust".to_string()), (".py".to_string(), "python".to_string())]);
    DiscoveredPlugins { manifests, routing }
}

struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let project = Self { dir: tempfile::tempdir().expect("failed to create a temp project root") };
        for (path, contents) in [
            ("Cargo.toml", "[package]\nname = \"krate\"\nversion = \"0.1.0\"\n"),
            ("src/lib.rs", LIB_RS),
            ("src/store.rs", STORE_RS),
            ("py/shapes.py", SHAPES_PY),
        ] {
            let full = project.root().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).expect("failed to create a fixture directory");
            std::fs::write(&full, contents).expect("failed to write the fixture");
        }
        project
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn walk(&self) -> Connection {
        let conn = open(self.root()).expect("failed to open the project index");
        schema::ensure_current(&conn, "rust-python-qualified-paths-test")
            .expect("failed to prepare the index");
        let conn = IndexStore::new(conn);
        let summary =
            bulk_index::run(self.root(), &conn, None, &rust_and_python()).expect("the bulk walk failed");
        assert!(summary.nodes > 0, "the walk produced no nodes at all");
        assert_eq!(summary.skipped_lines, 0, "a plugin emitted a line core could not read");
        conn.into_inner().unwrap()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

fn found(conn: &Connection, suffix: &str) -> Vec<String> {
    let mut names: Vec<String> =
        find_by_qualified_suffix(conn, suffix).unwrap().into_iter().map(|node| node.qualified_name).collect();
    names.sort();
    names
}

#[test]
fn rust_and_python_declarations_are_found_by_partial_path() {
    let project = Project::new();
    let conn = project.walk();

    // Every declaration's path survived ingest: an invalid one is stored NULL.
    let pathless: Vec<(String, String)> = conn
        .prepare(
            "SELECT qualifiedName, nativeKind FROM nodes \
             WHERE qualifiedPath IS NULL AND kind != 'File' \
             AND nativeKind NOT IN ('pending_symbol', 'reexport', 'resolved_module', 'external_module', 'container')",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get::<_, Option<String>>(1)?.unwrap_or_default())))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(pathless.is_empty(), "declarations stored without a path: {pathless:?}");

    // The trait-impl method, by its own segment and by its alias.
    assert_eq!(found(&conn, "<&'a Holder<T> as Show>::show"), vec!["store::<&'a Holder<T> as Show>::show"]);
    assert_eq!(found(&conn, "Holder::show"), vec!["store::<&'a Holder<T> as Show>::show"]);
    // A field and a same-named method: the separator tells them apart.
    assert_eq!(found(&conn, "Holder.inner"), vec!["store::Holder.inner"]);
    assert_eq!(found(&conn, "Holder::inner"), vec!["store::Holder::inner"]);

    // Python: a nested class's method by any boundary-anchored tail.
    assert_eq!(found(&conn, "Inner.m"), vec!["Outer.Inner.m"]);
    assert_eq!(found(&conn, "Outer.Inner.m"), Vec::<String>::new(), "a whole name is not its own suffix");
    assert_eq!(found(&conn, "Inner::m"), Vec::<String>::new());
}
