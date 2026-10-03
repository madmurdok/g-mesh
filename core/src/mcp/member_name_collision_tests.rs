//! A type member and a free declaration of the same name in one module,
//! walked by each real plugin and read back through `find_references`.
//!
//! Plugins store fields and methods in their module's container, beside the
//! free declarations, so a name-keyed placeholder addressed at the module
//! (`use m::y`, `m::y()`, Go `m.Y()`, Python `from a import y`) finds both.
//! The linker discards the members, which a module-scoped name never denotes,
//! and lands the edge on the free declaration. Members keep exactly the
//! usages that address them.

use crate::mcp::query_shapes::QueryShapes;
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::model::CallToolResult;

use crate::daemon::bulk_index;
use crate::daemon::manifest::{read_manifest, Capabilities, DiscoveredPlugins};
use crate::embedding::EmbeddingPipeline;
use crate::mcp::session_hints::SessionHints;
use crate::mcp::{find_callers_callees, find_references, SymbolQueryParams};
use crate::storage::connection::{open, project_dir};
use crate::storage::index_store::IndexStore;
use crate::storage::schema;

fn plugins(language: &str, ext: &str) -> DiscoveredPlugins {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins");
    let manifest = read_manifest(&dir.join(language)).expect("the checked-in plugin manifest");
    DiscoveredPlugins {
        manifests: HashMap::from([(language.to_string(), manifest)]),
        routing: HashMap::from([(ext.to_string(), language.to_string())]),
    }
}

/// A temp project walked by one real plugin and the linker. Its per-project
/// state directory is removed on drop.
struct Walked {
    dir: tempfile::TempDir,
    store: Arc<IndexStore>,
}

impl Walked {
    fn new(language: &str, ext: &str, files: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().expect("failed to create a temp project root");
        for (path, contents) in files {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, contents).unwrap();
        }
        let conn = open(dir.path()).expect("failed to open the project index");
        schema::ensure_current(&conn, "member-name-collision-test").expect("failed to prepare the index");
        let store = IndexStore::new(conn);
        let summary =
            bulk_index::run(dir.path(), &store, None, &plugins(language, ext)).expect("the bulk walk failed");
        assert!(summary.nodes > 0, "the walk produced no nodes");
        Self { dir, store: Arc::new(store) }
    }

    /// The id of the one declaration (not a placeholder) named `qualified_name`.
    fn declaration(&self, qualified_name: &str) -> String {
        let ids: Vec<String> = self
            .store
            .with(|conn| -> rusqlite::Result<_> {
                let mut stmt = conn.prepare(
                    "SELECT id FROM nodes WHERE qualifiedName = ?1 AND (nativeKind IS NULL OR nativeKind NOT IN \
                     ('pending_symbol', 'reexport', 'resolved_module', 'external_module', 'container'))",
                )?;
                let ids = stmt.query_map([qualified_name], |row| row.get(0))?.collect();
                ids
            })
            .unwrap();
        let [id] = ids.as_slice() else {
            panic!("expected one declaration named {qualified_name}, got {ids:?}");
        };
        id.clone()
    }

    fn references(&self, qualified_name: &str) -> serde_json::Value {
        let params = SymbolQueryParams {
            symbol_id: Some(self.declaration(qualified_name)),
            limit: Some(200),
            ..Default::default()
        };
        json_body(
            &find_references::handle(
                &self.store,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &HashMap::<String, Capabilities>::new(),
                &SessionHints::default(),
                params,
            )
            .unwrap(),
        )
    }

    /// What each `kind` edge from `from` points at, as `(qualifiedName,
    /// nativeKind)` - a placeholder shows up as `pending_symbol`.
    fn targets_of(&self, from: &str, kind: &str) -> BTreeSet<(String, String)> {
        let from = self.declaration(from);
        self.store
            .with(|conn| -> rusqlite::Result<_> {
                let mut stmt = conn.prepare(
                    "SELECT n.qualifiedName, COALESCE(n.nativeKind, '') FROM edges e JOIN nodes n ON n.id = e.toId \
                     WHERE e.fromId = ?1 AND e.kind = ?2",
                )?;
                let rows = stmt.query_map([from.as_str(), kind], |row| Ok((row.get(0)?, row.get(1)?)))?.collect();
                rows
            })
            .unwrap()
    }

    /// `qualified_name`'s usages as `(edge kind, using symbol or file)`,
    /// asserting the page is exact: every row resolved, no `unlinkedUsages`.
    fn exact_usages(&self, qualified_name: &str) -> BTreeSet<(String, String)> {
        let body = self.references(qualified_name);
        assert!(body.get("unlinkedUsages").is_none(), "{qualified_name}: {}", body["unlinkedUsages"]);
        assert_ne!(body["hasMore"], serde_json::json!(true), "{qualified_name}");
        body["results"]
            .as_array()
            .unwrap_or_else(|| panic!("{qualified_name}: no results array in {body}"))
            .iter()
            .map(|row| {
                assert_eq!(row["resolved"], serde_json::json!(true), "{qualified_name}: {row}");
                let from = row["qualifiedName"].as_str().or(row["filePath"].as_str()).unwrap_or_default();
                (row["referenceKind"].as_str().unwrap_or_default().to_string(), from.to_string())
            })
            .collect()
    }

    /// `qualified_name`'s callers through `find_callers`, as qualified names,
    /// asserting every row is resolved and the page is complete.
    fn callers(&self, qualified_name: &str) -> BTreeSet<String> {
        let params = SymbolQueryParams {
            symbol_id: Some(self.declaration(qualified_name)),
            limit: Some(200),
            ..Default::default()
        };
        let body = json_body(
            &find_callers_callees::handle_callers(
                &self.store,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &HashMap::<String, Capabilities>::new(),
                &SessionHints::default(),
                params,
            )
            .unwrap(),
        );
        assert_ne!(body["hasMore"], serde_json::json!(true), "{qualified_name}");
        body["results"]
            .as_array()
            .unwrap_or_else(|| panic!("{qualified_name}: no results array in {body}"))
            .iter()
            .map(|row| {
                assert_eq!(row["resolved"], serde_json::json!(true), "{qualified_name}: {row}");
                row["qualifiedName"].as_str().unwrap_or_default().to_string()
            })
            .collect()
    }
}

impl Drop for Walked {
    fn drop(&mut self) {
        if let Ok(state) = project_dir(self.dir.path()) {
            let _ = std::fs::remove_dir_all(state);
        }
    }
}

fn json_body(result: &CallToolResult) -> serde_json::Value {
    assert_ne!(result.is_error, Some(true), "expected a success result: {:?}", result.content);
    match &result.content[0] {
        rmcp::model::ContentBlock::Text(text) => serde_json::from_str(&text.text).unwrap(),
        other => panic!("expected text/json content, got {other:?}"),
    }
}

fn usages(rows: &[(&str, &str)]) -> BTreeSet<(String, String)> {
    rows.iter().map(|(kind, from)| (kind.to_string(), from.to_string())).collect()
}

/// (a) field `S.x` + `fn x`; (b) method `T::y` + `fn y`; field `U.z` + method
/// `U::z`, two members of one name.
const RUST_M: &str = r#"
pub struct S { pub x: u32 }
impl S {
    pub fn get_x(&self) -> u32 { self.x + x() }
}
pub fn x() -> u32 { 0 }

pub struct T;
impl T {
    pub fn y(&self) -> u32 { 1 }
    pub fn call_y(&self) -> u32 { self.y() + T::y(self) }
}
pub fn y() -> u32 { 2 }

pub struct U { pub z: u32 }
impl U {
    pub fn z(&self) -> u32 { self.z }
    pub fn both(&self) -> u32 { self.z + self.z() + U::z(self) }
}

pub fn local() -> u32 { x() + S { x: 1 }.x }
"#;

const RUST_PRELUDE: &str = "pub use crate::m::y;\n";

const RUST_USER: &str = r#"
use crate::m::{x, y, S, T, U};
use crate::m;

pub fn use_free() -> u32 { x() + y() + m::x() + m::y() }
pub fn via_prelude() -> u32 { crate::prelude::y() }
pub fn use_members(t: &T, u: &U) -> u32 { T::y(t) + U::z(u) + m::T::y(t) }
pub fn use_field(s: &S) -> u32 { s.x }
"#;

/// `use m::y`, `m::y()` and a `pub use` re-export of `y` land on the free fn,
/// not on the field or method of that name; the members keep their own
/// usages, and the field-vs-method pair of `U` stays as it is.
///
/// Control: return `Ok(None)` at the top of `Resolver::sole_non_member`
/// (`graph::symbol_links`): `m::x` and `m::y` lose their `src/user.rs` rows
/// and report `unlinkedUsages`.
#[test]
fn rust_a_module_scoped_name_lands_on_the_free_fn_beside_a_same_named_member() {
    let walked = Walked::new(
        "rust",
        ".rs",
        &[
            ("Cargo.toml", "[package]\nname = \"krate\"\nversion = \"0.1.0\"\n"),
            ("src/lib.rs", "pub mod m;\npub mod prelude;\npub mod user;\n"),
            ("src/m.rs", RUST_M),
            ("src/prelude.rs", RUST_PRELUDE),
            ("src/user.rs", RUST_USER),
        ],
    );

    assert_eq!(
        walked.exact_usages("m::x"),
        usages(&[
            ("CALLS", "m::S::get_x"),
            ("CALLS", "m::local"),
            ("CALLS", "user::use_free"),
            ("REFERENCES", "src/user.rs"),
        ])
    );
    assert_eq!(
        walked.exact_usages("m::y"),
        usages(&[
            ("CALLS", "user::use_free"),
            ("CALLS", "user::via_prelude"),
            ("REFERENCES", "src/prelude.rs"),
            ("REFERENCES", "src/user.rs"),
        ])
    );
    assert_eq!(
        walked.exact_usages("m::S.x"),
        usages(&[("REFERENCES", "m::S::get_x"), ("REFERENCES", "m::local")])
    );
    assert_eq!(
        walked.exact_usages("m::T::y"),
        usages(&[("CALLS", "m::T::call_y"), ("CALLS", "user::use_members")])
    );
    assert_eq!(
        walked.exact_usages("m::U.z"),
        usages(&[("REFERENCES", "m::U::z"), ("REFERENCES", "m::U::both")])
    );
    assert_eq!(
        walked.exact_usages("m::U::z"),
        usages(&[("CALLS", "m::U::both"), ("CALLS", "user::use_members")])
    );
}

/// A free `fn y` called bare from its own file, beside an inherent method
/// and a trait method `y`: the plugin binds the call to the free fn itself,
/// and the methods keep exactly their type-qualified callers.
///
/// Control: in the Rust plugin's `Declarer::declare` (`extractor::decls`),
/// record an associated item with `model.declare` like a free item - `m::y`
/// loses `m::local` and `m::T::call_y`.
#[test]
fn rust_a_same_file_bare_call_lands_on_the_free_fn_beside_same_named_methods() {
    let walked = Walked::new(
        "rust",
        ".rs",
        &[
            ("Cargo.toml", "[package]\nname = \"krate\"\nversion = \"0.1.0\"\n"),
            ("src/lib.rs", "pub mod m;\n"),
            (
                "src/m.rs",
                r#"
pub struct T;
impl T {
    pub fn y(&self) -> u32 { 1 }
    pub fn call_y(&self) -> u32 { y() + T::y(self) }
}
pub trait Tr {
    fn y() -> u32;
}
impl Tr for T {
    fn y() -> u32 { 3 }
}
pub fn y() -> u32 { 2 }
pub fn local() -> u32 { y() + <T as Tr>::y() }
"#,
            ),
        ],
    );

    assert_eq!(walked.callers("m::y"), BTreeSet::from(["m::T::call_y".to_string(), "m::local".to_string()]));
    assert_eq!(walked.callers("m::T::y"), BTreeSet::from(["m::T::call_y".to_string()]));
}

/// A trait-impl method is named `<S as Tr>::y`, whose parent is no type;
/// its alias `m::S::y` is what makes it a member.
const RUST_TRAIT_M: &str = r#"
pub struct S;
impl S {
    pub fn y() {}
}
impl crate::other::Tr for S {
    fn y() {}
}
"#;

const RUST_TRAIT_OTHER: &str = "pub trait Tr {\n    fn y();\n}\n\npub fn y() {}\n";

/// `m` re-exports `other::y` and holds an inherent and a trait-impl method
/// `y`: both are members, so `m::y()` has no candidate to link to in `m`
/// and stays unresolved - it must not land on the trait-impl method.
///
/// Control: drop the alias-suffix loop from `Resolver::is_type_member`
/// (`graph::symbol_links`) - the call lands on `m::<S as Tr>::y`.
#[test]
fn rust_a_name_beside_an_inherent_and_a_trait_impl_method_lands_on_neither() {
    let walked = Walked::new(
        "rust",
        ".rs",
        &[
            ("Cargo.toml", "[package]\nname = \"krate\"\nversion = \"0.1.0\"\n"),
            ("src/lib.rs", "pub mod m;\npub mod other;\npub mod user;\n"),
            ("src/m.rs", &format!("pub use crate::other::y;\n{RUST_TRAIT_M}")),
            ("src/other.rs", RUST_TRAIT_OTHER),
            ("src/user.rs", "pub fn call() {\n    crate::m::y();\n}\n"),
        ],
    );

    let targets = walked.targets_of("user::call", "CALLS");
    assert!(
        targets.iter().all(|(_, native_kind)| native_kind == "pending_symbol"),
        "m::y() must stay unresolved: {targets:?}"
    );
    assert_eq!(targets.len(), 1, "{targets:?}");
}

/// A free `fn y` beside a trait-impl method `y`: the free fn is the one
/// non-member.
///
/// Control: as above - both candidates count as non-members and the call
/// stays unresolved.
#[test]
fn rust_a_name_beside_a_trait_impl_method_lands_on_the_free_fn() {
    let walked = Walked::new(
        "rust",
        ".rs",
        &[
            ("Cargo.toml", "[package]\nname = \"krate\"\nversion = \"0.1.0\"\n"),
            ("src/lib.rs", "pub mod m;\npub mod other;\npub mod user;\n"),
            ("src/m.rs", "pub struct S;\nimpl crate::other::Tr for S {\n    fn y() {}\n}\npub fn y() {}\n"),
            ("src/other.rs", "pub trait Tr {\n    fn y();\n}\n"),
            ("src/user.rs", "pub fn call() {\n    crate::m::y();\n}\n"),
        ],
    );

    assert_eq!(
        walked.targets_of("user::call", "CALLS"),
        BTreeSet::from([("m::y".to_string(), "function".to_string())])
    );
}

/// Method `T.Y` + `func Y`, with `T` declared in another file of the
/// package than its method: the receiver type is found by container, not
/// by file.
const GO_TYPES: &str = "package m\n\ntype S struct{ X int }\n\ntype T struct{}\n";

const GO_M: &str = r#"package m

func (s S) GetX() int { return s.X + X() }

func X() int { return 0 }

func (t T) Y() int { return 1 }

func (t T) CallY() int { return t.Y() + Y() }

func Y() int { return 2 }

func Local() int { return X() + Y() }
"#;

const GO_USER: &str = r#"package user

import "example.com/probe/m"

func UseFree() int { return m.X() + m.Y() }

func UseMembers(t m.T, s m.S) int { return t.Y() + s.X }
"#;

/// `m.Y()` lands on `func Y`, not on method `T.Y` of the same package.
///
/// Controls: return `Ok(None)` at the top of `Resolver::sole_non_member`
/// (`graph::symbol_links`), or look the receiver type up in the member's
/// file instead of its container (`type_in_file` for every candidate): `Y`
/// loses `UseFree` and reports `unlinkedUsages`.
#[test]
fn go_a_package_qualified_call_lands_on_the_func_beside_a_same_named_method() {
    let walked = Walked::new(
        "go",
        ".go",
        &[
            ("go.mod", "module example.com/probe\n\ngo 1.22\n"),
            ("m/types.go", GO_TYPES),
            ("m/m.go", GO_M),
            ("user/user.go", GO_USER),
        ],
    );

    assert_eq!(
        walked.exact_usages("Y"),
        usages(&[("CALLS", "Local"), ("CALLS", "T.CallY"), ("CALLS", "UseFree")])
    );
    assert_eq!(
        walked.exact_usages("X"),
        usages(&[("CALLS", "Local"), ("CALLS", "S.GetX"), ("CALLS", "UseFree")])
    );
}

const PY_A: &str = r#"
class T:
    def y(self) -> int:
        return 1
    def call_y(self) -> int:
        return self.y() + T.y(self) + y()

def y() -> int:
    return 2

def local() -> int:
    return y()
"#;

const PY_B: &str = r#"
from pkg.a import y, T
from pkg import a

def use_free() -> int:
    return y() + a.y()

def use_members(t: T) -> int:
    return T.y(t)
"#;

/// `from pkg.a import y` and `y()` land on `def y`, not on method `T.y`.
///
/// Control: return `Ok(None)` at the top of `Resolver::sole_non_member`
/// (`graph::symbol_links`): `y` loses its `pkg/b.py` rows and reports
/// `unlinkedUsages`.
#[test]
fn python_an_imported_name_lands_on_the_function_beside_a_same_named_method() {
    let walked =
        Walked::new("python", ".py", &[("pkg/__init__.py", ""), ("pkg/a.py", PY_A), ("pkg/b.py", PY_B)]);

    assert_eq!(
        walked.exact_usages("y"),
        usages(&[
            ("CALLS", "T.call_y"),
            ("CALLS", "local"),
            ("CALLS", "use_free"),
            ("REFERENCES", "pkg/b.py"),
        ])
    );
    assert_eq!(walked.exact_usages("T.y"), usages(&[("CALLS", "T.call_y"), ("CALLS", "use_members")]));
}

const TS_A: &str = r#"
export class S {
  x = 1;
  getX(): number { return this.x + x(); }
}
export function x(): number { return 0; }

export class T {
  y(): number { return 1; }
  callY(): number { return this.y() + y(); }
}
export function y(): number { return 2; }

export function local(): number { return x() + y(); }
"#;

const TS_B: &str = r#"
import { x, y, T } from "./a";
import * as a from "./a";
export function useFree(): number { return x() + y() + a.x() + a.y(); }
export function useMembers(t: T): number { return t.y(); }
"#;

/// TypeScript's members are `file`-visible and `#`-qualified, so a
/// file-scoped name never reaches them and the type-member rule never runs:
/// these usages link with or without it.
#[test]
fn typescript_free_functions_beside_same_named_members_link_as_before() {
    let walked = Walked::new("typescript", ".ts", &[("src/a.ts", TS_A), ("src/b.ts", TS_B)]);

    assert_eq!(
        walked.exact_usages("x"),
        usages(&[("CALLS", "S#getX"), ("CALLS", "local"), ("CALLS", "useFree")])
    );
    assert_eq!(
        walked.exact_usages("y"),
        usages(&[("CALLS", "T#callY"), ("CALLS", "local"), ("CALLS", "useFree")])
    );
    assert_eq!(walked.exact_usages("T#y"), usages(&[("CALLS", "T#callY")]));
}
