//! Python's later binding of a module-level name, end to end: a project
//! extracted by the real Python plugin binary, sent over the real NDJSON
//! wire, committed by the real write path and linked by the real linker
//! (docs/architecture/gm-533-python-later-binding.md).
//!
//! - `pkg_star`: `def f`, then `from .b import *` with `b` declaring `f` -
//!   an in-file `f()` and `from pkg_star import f` elsewhere link `b.f`.
//! - `pkg_none`: the same, but `b` lacks `f` - both link the def.
//! - `pkg_named`: `def f`, then `from .a import f`, no `__all__` - both link
//!   `a.f`.
//! - `pkg_two`: `from .a import g`, then `from .c import g` - an in-file
//!   `g()` links `c.g`.
//!
//! Controls: C8 (`Bodies::resolve_bare` returns `Bound::Here` for
//! `ModuleBinding::DeclBeforeStar`) makes `pkg_star`'s in-file call link the
//! def; C1 (drop R1's rebinding check in `Resolver::walk_capped`) makes both
//! `pkg_star` calls link the def; C7 (`module_binding` ignores
//! `decl_starts`) makes both `pkg_named` calls link the def.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use g_mesh::daemon::bulk_index;
use g_mesh::daemon::manifest::{link_rules, read_manifest, DiscoveredPlugins};
use g_mesh::storage::connection::{open, project_dir};
use g_mesh::storage::index_store::IndexStore;
use g_mesh::storage::schema;
use rusqlite::Connection;

const RUN_F: &str = "\n\ndef run():\n    f()\n";

fn files() -> Vec<(&'static str, String)> {
    let def = "def f():\n    pass\n";
    vec![
        ("pkg_star/__init__.py", format!("{def}\nfrom .b import *\n{RUN_F}")),
        ("pkg_star/b.py", def.to_string()),
        ("pkg_none/__init__.py", format!("{def}\nfrom .b import *\n{RUN_F}")),
        ("pkg_none/b.py", "def g():\n    pass\n".to_string()),
        ("pkg_named/__init__.py", format!("{def}\nfrom .a import f\n{RUN_F}")),
        ("pkg_named/a.py", def.to_string()),
        ("pkg_two/__init__.py", "from .a import g\nfrom .c import g\n\n\ndef run():\n    g()\n".to_string()),
        ("pkg_two/a.py", "def g():\n    pass\n".to_string()),
        ("pkg_two/c.py", "def g():\n    pass\n".to_string()),
        (
            "user.py",
            "from pkg_star import f as star_f\nfrom pkg_none import f as none_f\n\
             from pkg_named import f as named_f\n\n\n\
             def go_star():\n    star_f()\n\n\ndef go_none():\n    none_f()\n\n\ndef go_named():\n    named_f()\n"
                .to_string(),
        ),
    ]
}

/// The Python plugin only, read from its checked-in manifest, whose
/// `${G_MESH_BIN_DIR}` resolves to this profile's `target/` directory.
fn python() -> DiscoveredPlugins {
    let plugins = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins");
    let manifest = read_manifest(&plugins.join("python")).expect("the checked-in Python manifest");
    DiscoveredPlugins {
        manifests: HashMap::from([("python".to_string(), manifest)]),
        routing: HashMap::from([(".py".to_string(), "python".to_string())]),
    }
}

struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let project = Self { dir: tempfile::tempdir().expect("failed to create a temp project root") };
        for (path, contents) in files() {
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
        schema::ensure_current(&conn, "python-later-binding-test").expect("failed to prepare the index");
        // Linked under the manifest's own rules, as `cli::init` and the
        // daemon link: without them Python has no `later_import_binds`.
        let plugins = python();
        let conn = IndexStore::new(conn).with_link_rules(link_rules(plugins.manifests.values()));
        let summary = bulk_index::run(self.root(), &conn, &plugins).expect("the bulk walk failed");
        assert!(summary.nodes > 0, "the walk produced no nodes at all");
        assert_eq!(summary.skipped_lines, 0, "the plugin emitted a line core could not read");
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

/// Every `CALLS` edge out of the function `function` of `file`, as `(target
/// file or "<pending>", resolved)`, sorted.
fn calls(conn: &Connection, file: &str, function: &str) -> Vec<(String, bool)> {
    let mut edges: Vec<(String, bool)> = conn
        .prepare(
            "SELECT CASE WHEN t.nativeKind = 'pending_symbol' THEN '<pending>' ELSE t.filePath END, \
                    e.resolved \
             FROM edges e JOIN nodes f ON f.id = e.fromId JOIN nodes t ON t.id = e.toId \
             WHERE f.filePath = ?1 AND f.name = ?2 AND e.kind = 'CALLS'",
        )
        .unwrap()
        .query_map([file, function], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    edges.sort();
    edges
}

#[test]
fn python_uses_link_the_later_binding_of_a_module_level_name() {
    let project = Project::new();
    let conn = project.walk();
    let linked = |file: &str| vec![(file.to_string(), true)];

    assert_eq!(
        calls(&conn, "pkg_star/__init__.py", "run"),
        linked("pkg_star/b.py"),
        "in-file, star provides f"
    );
    assert_eq!(calls(&conn, "user.py", "go_star"), linked("pkg_star/b.py"), "from pkg_star import f");
    assert_eq!(
        calls(&conn, "pkg_none/__init__.py", "run"),
        linked("pkg_none/__init__.py"),
        "in-file, star lacks f"
    );
    assert_eq!(calls(&conn, "user.py", "go_none"), linked("pkg_none/__init__.py"), "from pkg_none import f");
    assert_eq!(
        calls(&conn, "pkg_named/__init__.py", "run"),
        linked("pkg_named/a.py"),
        "in-file, displaced def"
    );
    assert_eq!(calls(&conn, "user.py", "go_named"), linked("pkg_named/a.py"), "from pkg_named import f");
    assert_eq!(calls(&conn, "pkg_two/__init__.py", "run"), linked("pkg_two/c.py"), "the later named import");
}
