//! Members reached through a re-export, end to end: a Rust crate and a
//! Python package extracted by the real plugin binaries, sent over the real
//! NDJSON wire, committed by the real write path and linked by the real
//! linker (docs/architecture/gm-472-reexport-links.md).
//!
//! Rust: `T.f` and `T::m` used through a named `pub use`, its `as` alias, a
//! glob and a glob-over-named chain. Python: `Cls.method()` with `Cls`
//! imported from a package whose `__init__` does `from .mod import *`. Every
//! usage must land on the declaration, and the head itself on its type.
//!
//! Control: make `Resolver::resolve` skip `through_head` - every member edge
//! stays unresolved.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use g_mesh::daemon::bulk_index;
use g_mesh::daemon::manifest::{read_manifest, DiscoveredPlugins};
use g_mesh::storage::connection::{open, project_dir};
use g_mesh::storage::index_store::IndexStore;
use g_mesh::storage::schema;
use rusqlite::Connection;

const FILES: &[(&str, &str)] = &[
    ("Cargo.toml", "[package]\nname = \"krate\"\nversion = \"0.1.0\"\n"),
    (
        "src/lib.rs",
        "pub mod a;\npub mod named;\npub mod glob;\npub mod outer;\npub mod user_named;\npub mod user_glob;\n\
         pub mod user_renamed;\npub mod user_outer;\n",
    ),
    ("src/a.rs", "pub struct T {\n    pub f: u32,\n}\n\nimpl T {\n    pub fn m(&self) -> u32 {\n        self.f\n    }\n}\n"),
    ("src/named.rs", "pub use crate::a::T;\npub use crate::a::T as Renamed;\n"),
    ("src/glob.rs", "pub use crate::a::*;\n"),
    ("src/outer.rs", "pub use crate::named::*;\n"),
    ("src/user_named.rs", "use crate::named::T;\n\npub fn run() -> u32 {\n    let t = T { f: 1 };\n    T::m(&t)\n}\n"),
    ("src/user_glob.rs", "use crate::glob::T;\n\npub fn run() -> u32 {\n    let t = T { f: 1 };\n    T::m(&t)\n}\n"),
    (
        "src/user_renamed.rs",
        "use crate::named::Renamed;\n\npub fn run() -> u32 {\n    let t = Renamed { f: 1 };\n    Renamed::m(&t)\n}\n",
    ),
    ("src/user_outer.rs", "use crate::outer::T;\n\npub fn run() -> u32 {\n    let t = T { f: 1 };\n    T::m(&t)\n}\n"),
    ("pkg/__init__.py", "from .mod import *\n"),
    ("pkg/mod.py", "class Cls:\n    def method(self):\n        pass\n"),
    ("user.py", "from pkg import Cls\n\n\ndef run():\n    Cls.method(None)\n"),
];

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
        for (path, contents) in FILES {
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
        schema::ensure_current(&conn, "reexport-member-linking-test").expect("failed to prepare the index");
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

/// Every `kind` edge out of `file`, as `(target qualifiedName or "<pending>",
/// resolved)`, sorted.
fn edges_from(conn: &Connection, file: &str, kind: &str) -> Vec<(String, bool)> {
    let mut edges: Vec<(String, bool)> = conn
        .prepare(
            "SELECT CASE WHEN t.nativeKind = 'pending_symbol' THEN '<pending>' ELSE t.qualifiedName END, \
                    e.resolved \
             FROM edges e JOIN nodes f ON f.id = e.fromId JOIN nodes t ON t.id = e.toId \
             WHERE f.filePath = ?1 AND e.kind = ?2",
        )
        .unwrap()
        .query_map([file, kind], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    edges.sort();
    edges
}

#[test]
fn members_used_through_a_reexport_link_to_their_declarations() {
    let project = Project::new();
    let conn = project.walk();

    for file in ["src/user_named.rs", "src/user_renamed.rs", "src/user_glob.rs", "src/user_outer.rs"] {
        assert_eq!(
            edges_from(&conn, file, "CALLS"),
            vec![("a::T::m".to_string(), true)],
            "{file}: the method"
        );
        let references = edges_from(&conn, file, "REFERENCES");
        assert!(references.contains(&("a::T.f".to_string(), true)), "{file}: the field, in {references:?}");
        assert!(
            references.iter().all(|(_, resolved)| *resolved),
            "{file}: every reference links, in {references:?}"
        );
    }

    let calls = edges_from(&conn, "user.py", "CALLS");
    assert_eq!(calls, vec![("Cls.method".to_string(), true)], "Python: the method through `import *`");
}
