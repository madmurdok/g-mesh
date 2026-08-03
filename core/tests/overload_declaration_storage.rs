//! The overload storage model, end to end: a fixture with two overload
//! signatures beside their implementation, extracted by the real plugin, sent
//! over the real NDJSON wire, and committed by the real write path - and the
//! rows that come out the far side.
//!
//! Everything below the plugin process is genuine (`daemon::bulk_index::run`
//! spawns it, `protocol::ndjson` parses its stream, `storage::write::apply_diff`
//! commits it). A daemon and a socket are deliberately *not*: the question
//! here is what a walk stores, not who asked for it, and the cold-start walk is
//! reachable on its own - the same entry point `g-mesh init` and
//! `g-mesh reindex` use.
//!
//! The negative half matters as much as the positive one. The design's central
//! promise is that a symbol with one declaration costs nothing at all, so this
//! suite asserts that an ordinary function in the very same file - walked by
//! the same plugin, through the same wire, into the same transaction - leaves
//! no declaration row behind, and that a plugin line for such a node never
//! mentions the field.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use g_mesh::daemon::bulk_index;
use g_mesh::storage::connection::{open, project_dir};
use g_mesh::storage::schema;
use rusqlite::Connection;

/// `parse` is written three times - two call signatures and the
/// implementation - and `plain` once, in the same file, as the control.
const OVERLOADS: &str = r#"/** Parses a value. */
export function parse(input: string): string[];
export function parse(input: number, radix?: number): number;
export function parse(input: string | number, radix?: number): any {
  return typeof input === "string" ? [input] : input;
}

export function plain(a: number): number {
  return a + 1;
}
"#;

struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let project = Self { dir: tempfile::tempdir().expect("failed to create a temp project root") };
        let path = project.root().join("src/overloads.ts");
        std::fs::create_dir_all(path.parent().unwrap()).expect("failed to create the fixture directory");
        std::fs::write(&path, OVERLOADS).expect("failed to write the fixture");
        project
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Runs the cold-start walk against this project and hands back the index
    /// it filled.
    fn walk(&self) -> Connection {
        let conn = open(self.root()).expect("failed to open the project index");
        schema::ensure_current(&conn, "overload-declaration-storage-test")
            .expect("failed to prepare the index");
        let conn = Mutex::new(conn);
        let summary = bulk_index::run(self.root(), &conn).expect("the bulk walk failed");
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

fn node_id(conn: &Connection, name: &str) -> String {
    conn.query_row("SELECT id FROM nodes WHERE name = ?1 AND kind = 'Function'", [name], |row| row.get(0))
        .unwrap_or_else(|e| panic!("no Function node named {name}: {e}"))
}

#[test]
fn an_overloaded_symbol_is_stored_as_one_node_with_every_declaration_it_was_written_as() {
    let project = Project::new();
    let conn = project.walk();

    let parse = node_id(&conn, "parse");

    // One node, as tsserver's own outline has it - not one per declaration.
    let parse_nodes: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes WHERE name = 'parse'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(parse_nodes, 1);

    let mut stmt = conn
        .prepare(
            "SELECT ordinal, startLine, startCol, endLine, endCol, signature, hasBody
             FROM declarations WHERE nodeId = ?1 ORDER BY ordinal",
        )
        .unwrap();
    let rows: Vec<(i64, i64, i64, i64, i64, Option<String>, bool)> = stmt
        .query_map([&parse], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();

    // Exactly the list the plugin holds in process (its own suite asserts the
    // same values off `extractFile`), ordinal by ordinal: source-ordered, each
    // with its own range and signature, and only the implementation with a
    // body.
    assert_eq!(
        rows,
        vec![
            (0, 1, 7, 1, 47, Some("parse(input: string): string[]".to_string()), false),
            (1, 2, 7, 2, 61, Some("parse(input: number, radix?: number): number".to_string()), false),
            (
                2,
                3,
                7,
                5,
                1,
                Some("parse(input: string | number, radix?: number): any".to_string()),
                true
            ),
        ]
    );

    // The node's own fields stay primary, and are filled the way TypeScript's
    // own tools fill them - the range from the implementation, the signature
    // from the first *call* signature, which is the one it shows a caller.
    let (signature, start_line, end_line): (Option<String>, i64, i64) = conn
        .query_row("SELECT signature, startLine, endLine FROM nodes WHERE id = ?1", [&parse], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    assert_eq!(signature.as_deref(), Some("parse(input: string): string[]"));
    assert_eq!((start_line, end_line), (3, 5));
}

#[test]
fn an_ordinary_symbol_in_the_same_walk_costs_no_declaration_rows() {
    let project = Project::new();
    let conn = project.walk();

    let plain = node_id(&conn, "plain");
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM declarations WHERE nodeId = ?1", [&plain], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0, "a single-declaration symbol must cost nothing");

    // And nothing else in the walk grew rows either - the File node, the
    // module, every edge endpoint. Only `parse` has any.
    let owners: Vec<String> = conn
        .prepare("SELECT DISTINCT nodeId FROM declarations")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(owners, vec![node_id(&conn, "parse")]);
}

#[test]
fn a_freshly_built_index_reads_schema_version_5() {
    let project = Project::new();
    let conn = project.walk();

    let version: String = conn
        .query_row("SELECT schema_version FROM meta WHERE id = 1", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, "5");
    assert_eq!(version, schema::CURRENT_SCHEMA_VERSION);
}

/// The wire half of the promise, asserted against the plugin's real stdout
/// rather than against core's serde: the line for a single-declaration node is
/// what it has always been, with no `declarations` key on it in any spelling -
/// not an empty array, not a null. A node that grew one would be a node whose
/// embedding and inbound edges churn on every reparse for nothing.
#[test]
fn the_plugins_own_ndjson_mentions_declarations_only_for_the_overloaded_node() {
    let project = Project::new();
    let entry = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins/js-ts/dist/src/index.js");
    let output = Command::new("node")
        .arg(&entry)
        .arg("--bulk-index")
        .arg(project.root())
        .output()
        .unwrap_or_else(|e| panic!("failed to run the plugin's bulk index at {}: {e}", entry.display()));
    assert!(output.status.success(), "the plugin's bulk index exited with {}", output.status);

    let stdout = String::from_utf8(output.stdout).expect("the plugin's stream must be UTF-8");
    let mentioning: Vec<&str> =
        stdout.lines().filter(|line| line.contains("declarations")).collect();

    assert_eq!(mentioning.len(), 1, "exactly one node in this file has more than one declaration");
    assert!(mentioning[0].contains("\"name\":\"parse\""), "{}", mentioning[0]);
    assert!(
        stdout.lines().any(|line| line.contains("\"name\":\"plain\"")),
        "the control node has to be in the stream for its silence to mean anything"
    );
}
