//! A path qualifier spelled like a type (`Keys::f()`) is a module
//! when this file declares a module of that name or the project's module
//! registry knows its key - Rust keeps modules and types in one namespace,
//! so the two cannot both be there. Only otherwise does the capital letter
//! decide "type".
//!
//! Its own module, so these tests stay apart from the parent file's; the
//! helpers are the parent's.

use g_mesh_plugin_sdk::wire::{EdgeKind, TargetKey, TargetScope};

use super::{container, Crate, Graph};

/// The `(scope, key)` of every placeholder a `CALLS` edge out of `from`
/// lands on, in edge order. Every caller below makes one call, so the order
/// is never asserted between two of them.
fn called_from(graph: &Graph, from: &str) -> Vec<(TargetScope, TargetKey)> {
    let from = graph.node(from).id.clone();
    graph
        .edges(EdgeKind::Calls)
        .into_iter()
        .filter(|edge| edge.from_id == from)
        .map(|edge| graph.target_of(graph.by_id(&edge.to_id)))
        .collect()
}

/// In the file that declares `mod Keys`, `Keys::f()` is a
/// direct, resolved edge onto the module's `f` - the file's own model says
/// `Keys` is a module, with no project registry involved.
///
/// Control: in `Bodies::resolve_path`, go back to `qualifier == "Self" ||
/// looks_like_type(qualifier)` for the type branch - `r` loses its edge.
#[test]
fn a_capitalized_inline_module_called_in_its_own_file_is_a_direct_edge() {
    let krate =
        Crate::new(&[("src/lib.rs", "pub mod Keys {\n    pub fn f() {}\n}\npub fn r() { Keys::f(); }\n")]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(graph.targets(EdgeKind::Calls, "r"), vec!["Keys::f"], "{:#?}", graph.names());
    assert!(graph.edges(EdgeKind::Calls).iter().all(|edge| edge.resolved), "a same-file target is resolved");
}

/// From another file, `Keys::f()` through `use
/// crate::Keys` and `crate::Keys::f()` written in full are both a `name`
/// placeholder in the module's own container `krate::Keys` - not a
/// `qualifiedName` `Keys::f` in `krate`, where nothing is declared.
///
/// Control: the same `resolve_path` revert - both become `qualifiedName`
/// placeholders in `krate`.
#[test]
fn a_capitalized_inline_module_called_from_another_file_is_a_name_key_in_that_module() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod b;\npub mod Keys {\n    pub fn f() {}\n}\n"),
        (
            "src/b.rs",
            "use crate::Keys;\npub fn imported() { Keys::f(); }\npub fn full() { crate::Keys::f(); }\n",
        ),
    ]);
    let graph = krate.extract("src/b.rs");
    let want = vec![(container("krate::Keys"), TargetKey::Name("f".into()))];
    assert_eq!(called_from(&graph, "b::imported"), want, "through the `use`");
    assert_eq!(called_from(&graph, "b::full"), want, "the full path");
}

/// `Keys::Key::Major(4)` after `use crate::Keys` continues
/// from the imported module, so the type-qualified tail is addressed in
/// `krate::Keys`, where `Keys::Key::Major` is declared.
///
/// Control: in `resolve_module_path`'s rule 4 (`extractor::keys`), drop
/// `&& !project.has_container(&key)` - the imported `Keys` is unresolved
/// again and the call becomes an open site with no placeholder.
#[test]
fn a_longer_path_through_an_imported_capitalized_module_continues_from_it() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod b;\npub mod Keys {\n    pub enum Key { Major(u8) }\n}\n"),
        ("src/b.rs", "use crate::Keys;\npub fn variant() { let _ = Keys::Key::Major(4); }\n"),
    ]);
    let graph = krate.extract("src/b.rs");
    assert_eq!(
        called_from(&graph, "b::variant"),
        vec![(container("krate::Keys"), TargetKey::QualifiedName("Keys::Key::Major".into()))],
        "{:#?}",
        graph.names()
    );
}

/// A local bound to `Keys::make()`, a free fn of a module
/// spelled like a type, is typed by `make`'s return type, so `k.m()` links.
#[test]
fn a_local_typed_by_a_capitalized_modules_free_fn_links_its_receiver_calls() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub mod Keys {
    pub struct K;
    impl K { pub fn m(&self) {} }
    pub fn make() -> K { K }
}
pub fn run() { let k = Keys::make(); k.m(); }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(
        graph.targets(EdgeKind::Calls, "run"),
        vec!["Keys::K::m", "Keys::make"],
        "{:#?}",
        graph.names()
    );
}

/// A written type `Keys::S` (a parameter annotation) and a
/// struct literal `Keys::S { .. }` both type their local as the module's
/// `S`, so `.m()` links.
///
/// Controls: in `Bodies::resolve_written`, go back to `qualifier == "Self"
/// || looks_like_type(qualifier)` - `annotated` loses its edge; the same in
/// `Bodies::struct_address` - `literal` loses its edge.
#[test]
fn a_written_type_and_a_struct_literal_through_a_capitalized_module_type_their_local() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub mod Keys {
    pub struct S { pub x: u8 }
    impl S { pub fn m(&self) {} }
}
pub fn annotated(s: Keys::S) { s.m(); }
pub fn literal() { let t = Keys::S { x: 1 }; t.m(); }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(graph.targets(EdgeKind::Calls, "annotated"), vec!["Keys::S::m"], "{:#?}", graph.names());
    assert_eq!(graph.targets(EdgeKind::Calls, "literal"), vec!["Keys::S::m"], "{:#?}", graph.names());
}

/// `use crate::a::inner;` where `inner` is an inline module
/// of `a.rs` gains the submodule's own `IMPORTS` edge, the same as a
/// file-backed submodule does.
///
/// Control: drop the `inline_modules` chain from `ProjectContext::rebuild` -
/// only `krate::a::*` is imported.
#[test]
fn a_use_of_an_inline_submodule_gains_its_imports_edge() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod a;\npub mod c;\n"),
        ("src/a.rs", "pub mod inner {\n    pub fn g() {}\n}\n"),
        ("src/c.rs", "use crate::a::inner;\npub fn run() { inner::g(); }\n"),
    ]);
    let graph = krate.extract("src/c.rs");
    assert_eq!(
        graph.targets(EdgeKind::Imports, "src/c.rs"),
        vec!["resolved_module krate::a::*".to_string(), "resolved_module krate::a::inner::*".to_string()],
        "{:#?}",
        graph.names()
    );
}

/// The documented limit: a capitalized module reached only
/// through a glob `use` has no import record, so `Keys::f()` is still read
/// as a type and addressed in the asking module, where nothing is declared.
/// A missing edge, never a wrong one: the address names no declaration.
#[test]
fn a_capitalized_module_reached_only_through_a_glob_stays_a_type_qualified_address() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod b;\npub mod Keys {\n    pub fn f() {}\n}\n"),
        ("src/b.rs", "use crate::*;\npub fn run() { Keys::f(); }\n"),
    ]);
    let graph = krate.extract("src/b.rs");
    assert_eq!(
        called_from(&graph, "b::run"),
        vec![(container("krate::b"), TargetKey::QualifiedName("b::Keys::f".into()))],
        "{:#?}",
        graph.names()
    );
}

/// A `mod Keys` inside a fn body, or a `pub mod
/// Gen` written in a `macro_rules!` body, is not the crate root's `Keys` /
/// `Gen`. The top-level structs of those names keep their type-qualified
/// callers from another file: `qualifiedName` placeholders in `krate`, not
/// `name` keys in a `krate::Keys` that holds nothing.
///
/// Control: in `module_tree::Scan::scan_file`, drop `&& decl.module_level`
/// from the inline-module filter - both calls become `Name("new")` in
/// `krate::Keys` / `krate::Gen`.
#[test]
fn a_mod_in_a_fn_or_macro_body_does_not_turn_a_same_named_types_calls_into_module_calls() {
    let krate = Crate::new(&[
        (
            "src/lib.rs",
            r#"pub mod b;
pub struct Keys;
impl Keys {
    pub fn new() {}
}
fn setup() {
    mod Keys {
        pub fn new() {}
    }
}
macro_rules! m {
    () => {
        pub mod Gen {
            pub fn f() {}
        }
    };
}
pub struct Gen;
impl Gen {
    pub fn new() {}
}
"#,
        ),
        (
            "src/b.rs",
            "use crate::Gen;\nuse crate::Keys;\npub fn rb() { Keys::new(); }\npub fn rg() { Gen::new(); }\n",
        ),
    ]);
    let graph = krate.extract("src/b.rs");
    assert_eq!(
        called_from(&graph, "b::rb"),
        vec![(container("krate"), TargetKey::QualifiedName("Keys::new".into()))]
    );
    assert_eq!(
        called_from(&graph, "b::rg"),
        vec![(container("krate"), TargetKey::QualifiedName("Gen::new".into()))]
    );
}
