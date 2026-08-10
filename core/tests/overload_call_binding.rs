//! The other half of `overload_declaration_storage.rs`. That suite proves a
//! symbol's declarations reach the `declarations` table; this one proves the
//! fact those declarations exist to carry: **which** of them a given call site
//! binds, stored on the edge as `toDeclaration`.
//!
//! Nothing here is faked. A real `g-mesh daemon`, the real JS/TS plugin, the
//! real semantic pass driving a real `tsserver` child, and the `edges` rows
//! read straight back out of the project's own SQLite index. That matters more
//! than usual for this fact: the structural layer cannot produce it at all - an
//! edge is identified by `(from, kind, to)`, so two calls of two different
//! overloads collapse onto one edge with nothing to tell them apart - so an
//! edge here carrying an ordinal can only have come from the checker, over the
//! wire, through `apply_semantic_pass` and `apply_diff`'s `toDeclaration`
//! column.
//!
//! Structured after `plugin_bridge.rs`'s
//! `an_ambiguous_reexport_is_resolved_by_the_plugin_semantic_pass`, including
//! its rewrite loop and the reason for it.
//!
//! Requires `plugins/typescript/dist/` to be up to date, which `core/build.rs` keeps
//! so.

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;
use rusqlite::Connection;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// Same budget as the ambiguous-barrel test's: this wait pays for a `tsserver`
/// child's startup and its first project load on top of everything the
/// structural path already costs.
const SEMANTIC_TIMEOUT: Duration = Duration::from_secs(60);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

/// Two call signatures and their implementation. Ordinal 0 takes a `string`,
/// ordinal 1 a `number`; ordinal 2 is the implementation TypeScript never shows
/// a caller, and so is the one no call may ever bind.
const LIB: &str = r#"export function parse(input: string): string[];
export function parse(input: number, radix?: number): number;
export function parse(input: string | number, radix?: number): unknown {
  return typeof input === "string" ? [input] : Number(input);
}
"#;

/// One caller per overload, so the answer is a *choice* and not a default: to a
/// name-matching walk these two calls are indistinguishable.
fn use_source(edit: usize) -> String {
    format!(
        r#"import {{ parse }} from "./lib";

export function useString(): string[] {{
  return parse("x") as string[];
}}

export function useNumber(): number {{
  return parse(10, 16);
}}
// edit {edit}
"#
    )
}

struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        Self { dir: tempfile::tempdir().expect("failed to create a temp project root") }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

fn spawn_daemon(root: &Path) -> Child {
    Command::new(BIN)
        .arg("daemon")
        .arg("--project-root")
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the daemon")
}

/// Every `(caller name, bound ordinal)` the index holds for a `CALLS` edge that
/// names a declaration - the whole shape this test is about.
fn bindings(db: &Path) -> Vec<(String, i64)> {
    let Ok(conn) = Connection::open(db) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT f.name, e.toDeclaration FROM edges e JOIN nodes f ON f.id = e.fromId
         WHERE e.kind = 'CALLS' AND e.toDeclaration IS NOT NULL
           AND e.source = 'ts-compiler' AND e.resolved = 1
         ORDER BY f.name",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?))) else {
        return Vec::new();
    };
    rows.collect::<rusqlite::Result<Vec<_>>>().unwrap_or_default()
}

#[test]
fn each_call_of_an_overloaded_function_is_stored_against_the_overload_it_binds() {
    let project = Project::new();
    let mut daemon = spawn_daemon(project.root());

    let pid_file = daemon::pid_path(project.root()).unwrap();
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while !pid_file.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for the daemon and its plugin to start");
        thread::sleep(Duration::from_millis(20));
    }

    let db_path = project_dir(project.root()).unwrap().join("index.db");

    // Rewritten until the index answers rather than written once: the watcher
    // is registered a moment after the cold-start walk, and a write landing in
    // that gap is missed outright rather than retried (the race
    // `semantic_pass_trigger.rs` documents). An unchanged file reparses to an
    // empty diff, so extra rounds cost nothing - hence the changing tail on
    // `use.ts`, which makes every round a real edit.
    let deadline = Instant::now() + SEMANTIC_TIMEOUT;
    let mut edits = 0;
    let found = loop {
        edits += 1;
        fs::write(
            project.root().join("tsconfig.json"),
            "{ \"compilerOptions\": { \"strict\": true } }\n",
        )
        .unwrap();
        fs::write(project.root().join("lib.ts"), LIB).unwrap();
        fs::write(project.root().join("use.ts"), use_source(edits)).unwrap();
        thread::sleep(Duration::from_millis(250));

        let found = bindings(&db_path);
        if found.len() == 2 {
            break found;
        }
        assert!(
            Instant::now() < deadline,
            "the semantic pass did not bind the overloaded calls after {edits} edit(s); \
             the index holds {found:?}"
        );
    };

    // The point of the whole exercise: two calls that a name-matching walk sees
    // as one, told apart, each against the signature TypeScript's own overload
    // resolution picked. Source order, so 0 is the `string` signature.
    assert_eq!(found, vec![("useNumber".to_string(), 1), ("useString".to_string(), 0)]);

    let conn = Connection::open(&db_path).unwrap();

    // Both bindings name one symbol - the node id is the *same* - so what
    // distinguishes them really is the ordinal and nothing else.
    let targets: Vec<String> = conn
        .prepare(
            "SELECT DISTINCT n.name || ':' || n.filePath FROM edges e JOIN nodes n ON n.id = e.toId
             WHERE e.kind = 'CALLS' AND e.toDeclaration IS NOT NULL",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(targets, vec!["parse:lib.ts".to_string()]);

    // And the ordinals are real addresses into the stored declaration list,
    // not free-floating numbers: each one is a row that exists, and neither is
    // the implementation.
    for (caller, ordinal) in &found {
        let has_body: bool = conn
            .query_row(
                "SELECT d.hasBody FROM declarations d
                 JOIN edges e ON e.toId = d.nodeId AND e.toDeclaration = d.ordinal
                 JOIN nodes f ON f.id = e.fromId
                 WHERE f.name = ?1",
                [caller],
                |row| row.get(0),
            )
            .unwrap_or_else(|e| panic!("{caller} bound ordinal {ordinal}, which has no row: {e}"));
        assert!(!has_body, "{caller} must bind a call signature, never the implementation");
    }

    // The collapsed edge the structural pass wrote was retracted rather than
    // left alongside: a caller of one overload has exactly one `CALLS` edge,
    // not one bound and one saying strictly less.
    let unbound: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges e JOIN nodes n ON n.id = e.toId
             WHERE e.kind = 'CALLS' AND e.toDeclaration IS NULL AND n.name = 'parse'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unbound, 0, "the collapsed edge must go out with the bound ones that replace it");

    let _ = daemon.kill();
    let _ = daemon.wait();
}
