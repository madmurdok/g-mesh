//! An edit applied through a plugin's control process leaves the edited
//! file's rows exactly as a full reindex of the edited tree would.
//!
//! Every case bulk-walks a fixture, then spawns a fresh control process, as
//! the daemon does: it has no baseline for any file the walk indexed. The
//! case edits the tree, sends `fileChanged` for every touched path (both
//! paths of a rename, as the watcher does), and compares the rows the edited
//! files own with a full reindex of the edited tree.
//!
//! A file owns its nodes, the edges out of them, their child-table rows and
//! its `indexed_files` row; `containers` is compared whole. Rows another file
//! owns that point at what the edit removed (a reference into a renamed
//! struct, an importer's placeholder for a deleted file) are left for that
//! file's next reparse, as `storage::write::apply_diff` documents, and are
//! not compared. Neither is the `indexed_files` row of a file that still
//! exists: watcher-applied edits leave baselines to query-time staleness.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use g_mesh::daemon::bulk_index;
use g_mesh::daemon::manifest::{read_manifest, DiscoveredPlugins, PluginManifest};
use g_mesh::daemon::plugin::{bundled_manifest, PluginProcess, BUNDLED_LANGUAGE};
use g_mesh::embedding::EmbeddingPipeline;
use g_mesh::storage::connection::{open, project_dir};
use g_mesh::storage::index_store::IndexStore;
use g_mesh::storage::schema;
use rusqlite::Connection;

fn plugin_manifest(language: &str) -> PluginManifest {
    let plugins = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins");
    read_manifest(&plugins.join(language)).unwrap_or_else(|err| panic!("{language} manifest: {err:#}"))
}

#[derive(Clone, Copy)]
enum Language {
    Rust,
    TypeScript,
    Go,
}

impl Language {
    fn manifest(self) -> PluginManifest {
        match self {
            Language::Rust => plugin_manifest("rust"),
            Language::TypeScript => bundled_manifest(),
            Language::Go => plugin_manifest("go"),
        }
    }

    fn discovered(self) -> DiscoveredPlugins {
        let (language, extension) = match self {
            Language::Rust => ("rust", ".rs"),
            Language::TypeScript => (BUNDLED_LANGUAGE, ".ts"),
            Language::Go => ("go", ".go"),
        };
        let routing = match self {
            Language::TypeScript => HashMap::new(),
            _ => HashMap::from([(extension.to_string(), language.to_string())]),
        };
        DiscoveredPlugins { manifests: HashMap::from([(language.to_string(), self.manifest())]), routing }
    }

    /// Whether the control process answers the per-file semantic pass after a
    /// reparse, and the reference index gets a whole-project one.
    fn semantic(self) -> bool {
        matches!(self, Language::Go)
    }
}

struct Project {
    dir: tempfile::TempDir,
    pid_dir: tempfile::TempDir,
    language: Language,
}

impl Project {
    fn new(language: Language, files: &[(&str, &str)]) -> Self {
        let project =
            Self { dir: tempfile::tempdir().unwrap(), pid_dir: tempfile::tempdir().unwrap(), language };
        for (path, text) in files {
            project.write(path, text);
        }
        project
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn write(&self, rel: &str, text: &str) {
        let path = self.root().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn remove(&self, rel: &str) {
        std::fs::remove_file(self.root().join(rel)).unwrap();
    }

    fn rename(&self, from: &str, to: &str) {
        std::fs::rename(self.root().join(from), self.root().join(to)).unwrap();
    }

    /// Backdates every file so the walk records its `indexed_files` baseline
    /// (it skips files modified within its margin).
    fn age(&self) {
        fn walk(dir: &Path, at: SystemTime) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, at);
                } else {
                    std::fs::File::options().write(true).open(&path).unwrap().set_modified(at).unwrap();
                }
            }
        }
        walk(self.root(), SystemTime::UNIX_EPOCH + Duration::from_secs(1_577_836_800));
    }

    fn wipe_state(&self) {
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(state);
        }
    }

    fn spawn(&self) -> PluginProcess {
        PluginProcess::spawn(self.root(), &self.language.manifest(), self.pid_dir.path().join("plugin.pid"))
            .expect("spawn the control process")
    }

    /// A full reindex from nothing, followed by the whole-project semantic
    /// pass for a language that has one.
    fn walk(&self) -> IndexStore {
        self.wipe_state();
        self.age();
        let conn = open(self.root()).unwrap();
        schema::ensure_current(&conn, "test").unwrap();
        let store = IndexStore::new(conn);
        bulk_index::run(self.root(), &store, None, &self.language.discovered()).expect("bulk walk");
        if self.language.semantic() {
            let plugin = self.spawn();
            plugin
                .semantic_pass(&store, Vec::new(), 16, &EmbeddingPipeline::disabled())
                .expect("semantic pass");
            plugin.shutdown(Duration::from_secs(5)).unwrap();
        }
        store
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        self.wipe_state();
    }
}

type Snapshot = Vec<(&'static str, BTreeSet<String>)>;

fn rows(conn: &Connection, table: &'static str, sql: &str) -> (&'static str, BTreeSet<String>) {
    let mut stmt = conn.prepare(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
    let columns = stmt.column_count();
    let set = stmt
        .query_map([], |row| {
            (0..columns)
                .map(|i| row.get::<_, rusqlite::types::Value>(i).map(|value| format!("{value:?}")))
                .collect::<rusqlite::Result<Vec<_>>>()
                .map(|parts| parts.join(" | "))
        })
        .unwrap()
        .collect::<rusqlite::Result<BTreeSet<_>>>()
        .unwrap();
    (table, set)
}

/// The rows `owned` files own, plus every row hanging on a node that no
/// longer exists (which is what a stale delete leaves behind).
fn snapshot(conn: &Connection, root: &Path, owned: &[&str]) -> Snapshot {
    let quoted = owned.iter().map(|path| format!("'{path}'")).collect::<Vec<_>>().join(", ");
    let gone = owned
        .iter()
        .filter(|path| !root.join(path).exists())
        .map(|path| format!("'{path}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let owned_nodes = format!("SELECT id FROM nodes WHERE filePath IN ({quoted})");
    let hangs =
        |column: &str| format!("{column} IN ({owned_nodes}) OR {column} NOT IN (SELECT id FROM nodes)");
    vec![
        rows(
            conn,
            "nodes",
            &format!(
                "SELECT id, kind, qualifiedName, filePath, nativeKind, startLine, startCol, endLine, endCol \
                 FROM nodes WHERE filePath IN ({quoted})"
            ),
        ),
        rows(
            conn,
            "edges",
            &format!(
                "SELECT e.id, e.kind, e.source, e.resolved, f.qualifiedName, t.qualifiedName FROM edges e \
                 LEFT JOIN nodes f ON f.id = e.fromId LEFT JOIN nodes t ON t.id = e.toId WHERE {}",
                hangs("e.fromId")
            ),
        ),
        rows(conn, "declarations", &format!("SELECT * FROM declarations WHERE {}", hangs("nodeId"))),
        rows(
            conn,
            "qualified_suffixes",
            &format!("SELECT * FROM qualified_suffixes WHERE {}", hangs("nodeId")),
        ),
        rows(
            conn,
            "placeholder_targets",
            &format!("SELECT * FROM placeholder_targets WHERE {}", hangs("nodeId")),
        ),
        rows(conn, "vectors", &format!("SELECT nodeId FROM vectors WHERE {}", hangs("nodeId"))),
        rows(conn, "containers", "SELECT * FROM containers"),
        rows(
            conn,
            "indexed_files",
            &format!("SELECT filePath FROM indexed_files WHERE filePath IN ({gone})"),
        ),
    ]
}

/// Every row in one snapshot and not the other, as `table: +incremental row`
/// / `table: +full row` lines.
fn differences(incremental: &Snapshot, full: &Snapshot) -> Vec<String> {
    let mut out = Vec::new();
    for ((table, inc), (_, full)) in incremental.iter().zip(full) {
        out.extend(inc.difference(full).map(|row| format!("{table}: +incremental {row}")));
        out.extend(full.difference(inc).map(|row| format!("{table}: +full {row}")));
    }
    out
}

type Step<'a> = (&'a dyn Fn(&Project), &'a [&'a str]);

struct Case<'a> {
    language: Language,
    files: &'a [(&'a str, &'a str)],
    /// Reparse these first, so the control process has a baseline for them.
    warm: &'a [&'a str],
    /// Each edit, then the paths the watcher reports for it, in order.
    steps: &'a [Step<'a>],
    /// The files whose rows are compared.
    owned: &'a [&'a str],
}

/// Runs `case` and asserts the edited files' rows match a full reindex.
/// Returns the full reindex's snapshot, for case-specific assertions.
fn assert_matches_full_reindex(case: Case<'_>) -> Snapshot {
    let project = Project::new(case.language, case.files);
    let store = project.walk();
    let plugin = project.spawn();
    let embedding = EmbeddingPipeline::disabled();
    let semantic_suspended = !case.language.semantic();
    if case.language.semantic() {
        plugin.semantic_pass(&store, Vec::new(), 16, &embedding).expect("semantic pass");
    }
    for path in case.warm {
        plugin.apply_file_change(&store, *path, &embedding, semantic_suspended).unwrap();
    }
    for (edit, changed) in case.steps {
        edit(&project);
        for path in *changed {
            plugin.apply_file_change(&store, *path, &embedding, semantic_suspended).unwrap();
        }
    }
    plugin.shutdown(Duration::from_secs(5)).unwrap();

    let incremental = snapshot(&store.into_inner().unwrap(), project.root(), case.owned);
    let full = snapshot(&project.walk().into_inner().unwrap(), project.root(), case.owned);
    let differences = differences(&incremental, &full);
    assert!(
        differences.is_empty(),
        "the edited files' rows differ from a full reindex:\n{}",
        differences.join("\n")
    );
    full
}

fn count(snapshot: &Snapshot, table: &str) -> usize {
    snapshot.iter().find(|(name, _)| *name == table).map_or(0, |(_, rows)| rows.len())
}

// --- Rust, through the SDK -------------------------------------------------

const RUST_FILES: &[(&str, &str)] = &[
    ("Cargo.toml", "[package]\nname = \"krate\"\nversion = \"0.1.0\"\n"),
    ("src/lib.rs", "pub mod a;\npub mod b;\n"),
    (
        "src/a.rs",
        "pub struct KD { pub f: u8 }\nimpl KD {\n    pub fn m(&self) -> u8 { self.f }\n}\n\
         pub fn use_kd(k: &KD) -> u8 { k.m() }\n",
    ),
    ("src/b.rs", "use crate::a::KD;\npub fn other(k: &KD) -> u8 { crate::a::use_kd(k) }\n"),
];

#[test]
fn rust_deleted_file_through_a_cold_process() {
    assert_matches_full_reindex(Case {
        language: Language::Rust,
        files: RUST_FILES,
        warm: &[],
        steps: &[(&|project| project.remove("src/b.rs"), &["src/b.rs"])],
        owned: &["src/b.rs"],
    });
}

#[test]
fn rust_deleted_file_through_a_warm_process() {
    assert_matches_full_reindex(Case {
        language: Language::Rust,
        files: RUST_FILES,
        warm: &["src/b.rs"],
        steps: &[(&|project| project.remove("src/b.rs"), &["src/b.rs"])],
        owned: &["src/b.rs"],
    });
}

#[test]
fn rust_file_renamed_away_through_a_cold_process() {
    let full = assert_matches_full_reindex(Case {
        language: Language::Rust,
        files: RUST_FILES,
        warm: &[],
        steps: &[(&|project| project.rename("src/b.rs", "src/c.rs"), &["src/b.rs", "src/c.rs"])],
        owned: &["src/b.rs", "src/c.rs"],
    });
    assert!(count(&full, "nodes") > 0, "the renamed file's rows are compared, not only the old path's");
}

#[test]
fn rust_declaration_renamed_through_a_cold_process() {
    let full = assert_matches_full_reindex(Case {
        language: Language::Rust,
        files: RUST_FILES,
        warm: &[],
        steps: &[(
            &|project| {
                project.write(
                    "src/a.rs",
                    "pub struct KD2 { pub f: u8 }\nimpl KD2 {\n    pub fn m(&self) -> u8 { self.f }\n}\n\
                 pub fn use_kd(k: &KD2) -> u8 { k.m() }\n",
                )
            },
            &["src/a.rs"],
        )],
        owned: &["src/a.rs"],
    });
    assert!(count(&full, "qualified_suffixes") > 0, "the fixture exercises qualified suffixes");
}

// --- TypeScript ------------------------------------------------------------

const TS_FILES: &[(&str, &str)] = &[
    ("a.ts", "import { gone } from \"./b\";\nexport function keep(): number { return gone(); }\n"),
    ("b.ts", "export function gone(): number { return 1; }\nexport function KD(): number { return 2; }\n"),
];

#[test]
fn typescript_deleted_file_through_a_warm_process() {
    assert_matches_full_reindex(Case {
        language: Language::TypeScript,
        files: TS_FILES,
        warm: &["a.ts", "b.ts"],
        steps: &[(&|project| project.remove("b.ts"), &["b.ts"])],
        owned: &["b.ts"],
    });
}

#[test]
fn typescript_file_deleted_and_restored_through_a_warm_process() {
    let full = assert_matches_full_reindex(Case {
        language: Language::TypeScript,
        files: TS_FILES,
        warm: &["b.ts"],
        steps: &[
            (&|project| project.remove("b.ts"), &["b.ts"]),
            (&|project| project.write("b.ts", TS_FILES[1].1), &["b.ts"]),
        ],
        owned: &["b.ts"],
    });
    assert!(count(&full, "nodes") > 0);
}

#[test]
fn typescript_declaration_renamed_through_a_cold_process() {
    assert_matches_full_reindex(Case {
        language: Language::TypeScript,
        files: TS_FILES,
        warm: &[],
        steps: &[(
            &|project| {
                project.write(
                "b.ts",
                "export function gone(): number { return 1; }\nexport function KD2(): number { return 2; }\n",
            )
            },
            &["b.ts"],
        )],
        owned: &["b.ts"],
    });
}

// --- Go, with its semantic tier -------------------------------------------

/// `h.Name()` is a call through a variable receiver: the structural tier
/// leaves it open and the semantic pass answers it with a placeholder in this
/// file and a `semantic` edge onto it. A complete structural diff deletes
/// that placeholder; the semantic pass that follows the reparse re-sends it.
const GO_FILES: &[(&str, &str)] = &[
    ("go.mod", "module probe\n\ngo 1.21\n"),
    (
        "a.go",
        "package probe\n\ntype Handle struct{ name string }\n\nfunc (h *Handle) Name() string { return h.name }\n\n\
         func newHandle() *Handle { return &Handle{name: \"h\"} }\n\nfunc varReceiver() string {\n\th := newHandle()\n\
         \treturn h.Name()\n}\n\ntype KD struct{ F int }\n\nfunc (k KD) M() int { return k.F }\n",
    ),
];

#[test]
fn go_declaration_renamed_through_a_cold_process_keeps_its_semantic_rows() {
    let full = assert_matches_full_reindex(Case {
        language: Language::Go,
        files: GO_FILES,
        warm: &[],
        steps: &[(&|project| project.write("a.go", &GO_FILES[1].1.replace("KD", "KD2")), &["a.go"])],
        owned: &["a.go"],
    });
    let (_, edges) = full.iter().find(|(name, _)| *name == "edges").unwrap();
    assert!(
        edges.iter().any(|row| row.contains("Text(\"semantic\")")),
        "the fixture must give a.go semantic edges, or this case proves nothing about them: {edges:#?}"
    );
}
