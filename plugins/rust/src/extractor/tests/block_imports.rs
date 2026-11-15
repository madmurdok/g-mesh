//! `IMPORTS` edges for the module dependencies a `use` at module level does
//! not show: a `use` inside a fn body, closure or block, and a module path
//! written inline (`crate::a::b::f()`), in a macro's arguments, in a trait
//! clause, a supertrait bound, a qualified self or an attribute.
//!
//! Its own module, so these tests stay apart from the parent file's; the
//! helpers are the parent's.

use g_mesh_plugin_sdk::wire::EdgeKind;

use super::{reexport_rows, Crate};

/// A crate whose module `c` is the file under test and whose other modules
/// are the targets: `a` (with the child module `a::b`), `d`.
fn crate_with_c(c: &str) -> Crate {
    Crate::new(&[
        ("src/lib.rs", "pub mod a;\npub mod c;\npub mod d;\n"),
        (
            "src/a.rs",
            "pub mod b;\npub fn f() {}\npub struct T;\nimpl T { pub fn new() -> T { T } }\n\
             pub trait Tr { fn m(&self); }\npub trait Sup {}\n",
        ),
        ("src/a/b.rs", "pub fn f() {}\n"),
        ("src/d.rs", "pub struct S2;\n"),
        ("src/c.rs", c),
    ])
}

/// The distinct `IMPORTS` targets of `src/c.rs`, sorted.
fn imported_by_c(c: &str) -> Vec<String> {
    let krate = crate_with_c(c);
    let graph = krate.extract("src/c.rs");
    let mut targets = graph.targets(EdgeKind::Imports, "src/c.rs");
    targets.dedup();
    targets
}

fn modules(keys: &[&str]) -> Vec<String> {
    keys.iter().map(|key| format!("resolved_module krate::{key}::*")).collect()
}

/// A `use` in a closure, a fn body or a nested block imports like a module
/// level one (`get_dependencies` is answered from `IMPORTS` edges).
///
/// Control: make `Bodies::visit`'s `use_declaration` arm do nothing - every
/// case below reads empty.
#[test]
fn a_use_inside_a_closure_fn_body_or_block_draws_the_imports_edge() {
    let cases = [
        ("closure", "pub fn go() { let g = || { use crate::a::f; f() }; g(); }\n", vec!["a"]),
        ("fn body", "pub fn go() { use crate::a::b::f; f() }\n", vec!["a::b"]),
        ("nested block", "pub fn go() { { use crate::a::T; let _ = T; } }\n", vec!["a"]),
    ];
    for (label, source, expected) in cases {
        assert_eq!(imported_by_c(source), modules(&expected), "{label}");
    }
}

/// A module-level `use` still draws exactly its own edges, and a block `use`
/// binds no name in the module (no `reexport` for it).
///
/// Control: have `Declarer` skip module-level `use` - the first assertion
/// reads empty.
#[test]
fn a_module_level_use_is_unchanged() {
    let krate = crate_with_c("pub use crate::a::f;\npub fn go() { f() }\n");
    let graph = krate.extract("src/c.rs");
    assert_eq!(graph.targets(EdgeKind::Imports, "src/c.rs"), modules(&["a"]));
    assert_eq!(reexport_rows(&graph).len(), 1);

    let krate = crate_with_c("pub fn go() { use crate::a::f; f() }\n");
    let graph = krate.extract("src/c.rs");
    assert_eq!(graph.targets(EdgeKind::Imports, "src/c.rs"), modules(&["a"]));
    assert!(reexport_rows(&graph).is_empty(), "a block `use` binds no name in the module");
}

/// `crate::a::b::f()` imports `a::b`; `crate::a::T::new()` imports `a`, the
/// type segment stripped.
///
/// Control: make `Bodies::path_import` return at once - both read empty.
/// Control for the type strip: skip `is_type_qualifier` there - the second
/// stops being exactly `a`.
#[test]
fn an_inline_module_path_imports_the_module_it_goes_through() {
    assert_eq!(imported_by_c("pub fn go() { crate::a::b::f(); }\n"), modules(&["a::b"]));
    assert_eq!(imported_by_c("pub fn go() { let _ = crate::a::T::new(); }\n"), modules(&["a"]));
}

/// A path through a child module (`b::f()` in `a`) imports it; a path
/// through the file's own module imports nothing.
///
/// Control: drop the `container == module.key` test in `path_import` - the
/// own-module case gains an edge onto itself.
#[test]
fn a_child_module_path_imports_the_child_and_an_own_module_path_nothing() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod a;\n"),
        ("src/a.rs", "pub mod b;\npub fn f() {}\npub fn run() { b::f(); }\n"),
        ("src/a/b.rs", "pub fn f() {}\n"),
    ]);
    let graph = krate.extract("src/a.rs");
    assert_eq!(graph.targets(EdgeKind::Imports, "src/a.rs"), modules(&["a::b"]));

    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod a;\n"),
        ("src/a.rs", "pub fn f() {}\npub fn run() { crate::a::f(); self::f(); }\n"),
    ]);
    let graph = krate.extract("src/a.rs");
    assert!(graph.targets(EdgeKind::Imports, "src/a.rs").is_empty(), "{:#?}", graph.names());
}

/// Paths in a macro's arguments, nested macros included, import their
/// modules (a token tree has no path nodes).
///
/// Control: drop the `token_tree` loop after the `match` in `Bodies::visit`'s
/// expression walk - the macro cases read empty.
#[test]
fn a_path_in_macro_arguments_imports_its_module() {
    assert_eq!(
        imported_by_c("pub fn go() { println!(\"{}\", crate::a::T::new()); }\n"),
        modules(&["a"]),
        "a type-qualified path in a macro"
    );
    assert_eq!(
        imported_by_c("pub fn go() { let _ = vec![format!(\"{}\", crate::a::b::f())]; }\n"),
        modules(&["a::b"]),
        "a path inside a nested macro"
    );
    assert_eq!(
        imported_by_c("pub fn go() { println!(\"{} {}\", crate::a::f(), crate::d::S2); }\n"),
        modules(&["a", "d"])
    );
}

/// An impl's trait clause, its self type and a supertrait bound each draw an
/// edge.
///
/// Control: remove the two `nested_path_imports` calls in the impl branch of
/// `Bodies` - the first two cases read empty; remove the one in
/// `supertype_to` - the third does.
#[test]
fn impl_clauses_and_supertrait_bounds_import_their_modules() {
    assert_eq!(
        imported_by_c("pub struct L;\nimpl crate::a::Tr for L { fn m(&self) {} }\n"),
        modules(&["a"]),
        "trait clause"
    );
    assert_eq!(imported_by_c("impl crate::d::S2 { pub fn z() {} }\n"), modules(&["d"]), "self type");
    assert_eq!(imported_by_c("pub trait X: crate::a::Sup {}\n"), modules(&["a"]), "supertrait bound");
}

/// `<S as crate::a::Tr>::m()` imports the paths on both sides of `as`.
///
/// Control: remove the `nested_path_imports` loop in `Bodies::path_use`'s
/// `flatten_path` fallback - reads empty.
#[test]
fn a_qualified_self_path_imports_both_of_its_paths() {
    assert_eq!(
        imported_by_c("pub fn go(s: &crate::d::S2) { <crate::d::S2 as crate::a::Tr>::m(s); }\n"),
        modules(&["a", "d"])
    );
}

/// `pub(in crate::a)` names a visibility, not a dependency.
///
/// Control: drop the `visibility_modifier` guard in `path_import` - an edge
/// onto `a` appears.
#[test]
fn a_restricted_visibility_path_draws_no_edge() {
    assert!(imported_by_c("pub(in crate::a) fn h() {}\n").is_empty());
}

/// `#[derive(crate::a::D)]` and an attribute's own path and arguments import
/// their modules.
///
/// Control: drop the `attribute_imports` call in `Bodies::visit` - reads
/// empty.
#[test]
fn attribute_paths_and_arguments_import_their_modules() {
    assert_eq!(imported_by_c("#[derive(crate::a::D)]\npub struct X;\n"), modules(&["a"]), "derive argument");
    assert_eq!(
        imported_by_c("#[crate::d::attr(crate::a::arg)]\npub fn q() {}\n"),
        modules(&["a", "d"]),
        "attribute path and argument"
    );
}
