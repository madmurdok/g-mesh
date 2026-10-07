//! Function bodies through the full extractor: the lexical scope chain, the
//! `CALLS` and `REFERENCES` edges calls and names become, and the
//! `SUPERTYPE_OF` edges heritage clauses become.

mod common;

use common::{extract, extract_in, Fixture};
use g_mesh_plugin_sdk::wire::EdgeKind::{self, Calls, Imports, References, SupertypeOf};
use g_mesh_plugin_sdk::{Extractor, RelPath};
use g_mesh_plugin_typescript::extractor::TypeScriptExtractor;

const PATH: &str = "src/a.ts";

// --- through a real project --------------------------------------------------

#[test]
fn a_call_of_an_imported_function_calls_its_pending_symbol_at_the_resolved_path() {
    let graph = extract_in(
        &[
            ("src/a.ts", "import { f } from \"./b\";\nexport function g() {\n  f();\n}\n"),
            ("src/b.ts", "export function f() {}\n"),
        ],
        "src/a.ts",
    );
    assert_edge!(graph, Imports, "<file>", "resolved_module:src/b.ts");
    assert_edge!(graph, Calls, "g", "pending_symbol:src/b.ts#f");
    assert_eq!(graph.pairs(Calls).len(), 1, "{:?}", graph.pairs(Calls));
}

#[test]
fn an_import_resolves_once_its_target_is_reported_present() {
    let fixture = Fixture::new(&[("src/a.ts", "import { f } from \"./b\";\nf();\n")]);
    let mut project = fixture.load();

    let before = fixture.extract(&project, "src/a.ts");
    assert_edge!(before, Imports, "<file>", "external_module:./b");
    assert!(
        !before.labels().iter().any(|label| label.starts_with("resolved_module:")),
        "{:?}",
        before.labels()
    );

    // Never written to disk: only the hook makes it exist.
    TypeScriptExtractor.file_presence_changed(&mut project, &RelPath::new("src/b.ts"), true);
    let after = fixture.extract(&project, "src/a.ts");
    assert_edge!(after, Imports, "<file>", "resolved_module:src/b.ts");
    assert!(
        !after.labels().iter().any(|label| label.starts_with("external_module:")),
        "{:?}",
        after.labels()
    );
}

// --- scope -------------------------------------------------------------------

#[test]
fn a_parameter_shadows_a_file_level_function() {
    let graph = extract(PATH, "function f() {}\nfunction g(f) { f(); }\nfunction h() { f(); }\n");
    assert_no_edge!(graph, Calls, "g", "f");
    assert_no_edge!(graph, References, "g", "f");
    assert_edge!(graph, Calls, "h", "f");
}

#[test]
fn a_hoisted_var_or_nested_function_shadows_across_the_whole_body() {
    let graph = extract(
        PATH,
        "function f() {}\n\
         function g() { f(); if (true) { var f = 1; } }\n\
         function h() { f(); function f() {} }\n\
         function k() { f(); }\n",
    );
    assert_no_edge!(graph, Calls, "g", "f");
    assert_no_edge!(graph, Calls, "h", "f");
    assert_edge!(graph, Calls, "k", "f");
}

#[test]
fn a_block_let_or_const_shadows_only_inside_its_block() {
    let graph = extract(
        PATH,
        "function f() {}\n\
         function g() { { const f = 1; f(); } }\n\
         function h() { { let f = 1; } f(); }\n",
    );
    assert_no_edge!(graph, Calls, "g", "f");
    assert_edge!(graph, Calls, "h", "f");
}

#[test]
fn a_switch_body_is_one_scope() {
    let graph = extract(
        PATH,
        "function f() {}\n\
         function g(x) { switch (x) { case 1: const f = 1; break; case 2: f(); } }\n\
         function h(x) { switch (x) { case 1: break; } f(); }\n",
    );
    assert_no_edge!(graph, Calls, "g", "f");
    assert_edge!(graph, Calls, "h", "f");
}

#[test]
fn a_catch_parameter_is_bound_only_in_its_handler() {
    let graph = extract(
        PATH,
        "function f() {}\n\
         function g() { try {} catch (f) { f(); } }\n\
         function h() { try {} catch (f) {} f(); }\n",
    );
    assert_no_edge!(graph, Calls, "g", "f");
    assert_edge!(graph, Calls, "h", "f");
}

#[test]
fn a_for_of_declaration_binds_its_variable_but_not_its_subject() {
    let graph = extract(
        PATH,
        "function f() {}\n\
         function g(xs) { for (const f of xs) { f(); } }\n\
         function h() { for (const f of f()) {} }\n",
    );
    assert_no_edge!(graph, Calls, "g", "f");
    assert_edge!(graph, Calls, "h", "f");
}

#[test]
fn a_for_of_without_a_declaration_binds_nothing() {
    let graph = extract(PATH, "function f() {}\nfunction g(xs) { for (f of xs) { f(); } }\n");
    assert_edge!(graph, Calls, "g", "f");
}

#[test]
fn destructuring_and_defaults_bind_only_their_left_hand_side() {
    let graph = extract(
        PATH,
        "function f() {}\n\
         function a() {}\n\
         function g({ a: f }) { a(); f(); }\n\
         function h([f]) { f(); }\n\
         function m() { const { f } = {}; f(); }\n",
    );
    assert_no_edge!(graph, Calls, "g", "f");
    assert_edge!(graph, Calls, "g", "a");
    assert_no_edge!(graph, Calls, "h", "f");
    assert_no_edge!(graph, Calls, "m", "f");

    // The JavaScript grammar writes a defaulted parameter as an assignment
    // pattern, whose right-hand side is a use.
    let defaulted = extract("src/a.js", "function d() {}\nfunction k(y = d) { y(); }\n");
    assert_edge!(defaulted, References, "k", "d");
}

#[test]
fn a_default_inside_an_object_pattern_parameter_is_a_call_of_the_function() {
    let graph = extract(PATH, "function d() {}\nfunction k({ y = d() }) {}\n");
    assert_edge!(graph, Calls, "k", "d");

    // The JavaScript grammar writes the parameter as a bare pattern.
    let js = extract("src/a.js", "function d() {}\nfunction k({ y = d() }) {}\n");
    assert_edge!(js, Calls, "k", "d");
}

#[test]
fn a_default_inside_an_array_pattern_parameter_is_a_call_of_the_function() {
    let graph = extract(PATH, "function d() {}\nfunction k([y = d()]) {}\n");
    assert_edge!(graph, Calls, "k", "d");
}

#[test]
fn a_name_default_inside_a_pattern_parameter_is_a_reference() {
    let source = "function z() {}\nfunction k({ y = z }) {}\n";
    assert_edge!(extract(PATH, source), References, "k", "z");
    assert_edge!(extract("src/a.js", source), References, "k", "z");
}

#[test]
fn a_default_inside_a_nested_pattern_parameter_is_walked() {
    let graph = extract(PATH, "function d() {}\nfunction k({ a: { b = d() } }) {}\n");
    assert_edge!(graph, Calls, "k", "d");
}

#[test]
fn a_computed_key_inside_a_pattern_parameter_is_walked() {
    let graph = extract(PATH, "function f() {}\nfunction k({ [f()]: y }) {}\n");
    assert_edge!(graph, Calls, "k", "f");
}

#[test]
fn the_name_a_defaulted_pattern_binds_is_not_a_use() {
    let graph = extract(PATH, "function y() {}\nfunction d() {}\nfunction k({ y = d() }) {}\n");
    assert_edge!(graph, Calls, "k", "d");
    assert_no_edge!(graph, Calls, "k", "y");
    assert_no_edge!(graph, References, "k", "y");
}

#[test]
fn a_destructured_sibling_shadows_a_default_like_a_plain_parameter() {
    let graph = extract(
        PATH,
        "function a() {}\n\
         function s({ a, b = a() }) {}\n\
         function u(a, b = a()) {}\n",
    );
    assert_no_edge!(graph, Calls, "s", "a");
    assert_no_edge!(graph, References, "s", "a");
    assert_no_edge!(graph, Calls, "u", "a");
    assert_no_edge!(graph, References, "u", "a");
}

#[test]
fn a_named_functions_own_name_is_not_bound_so_recursion_keeps_its_edge() {
    let graph = extract(PATH, "function f() { f(); }\nconst e = function e() { e(); };\n");
    assert_edge!(graph, Calls, "f", "f");
    assert_edge!(graph, Calls, "e", "e");
}

// --- calls ---------------------------------------------------------------------

#[test]
fn a_call_at_top_level_references_its_target_from_the_file() {
    let graph = extract(PATH, "function f() {}\nf();\n");
    assert_edge!(graph, References, "<file>", "f");
    assert!(graph.pairs(Calls).is_empty(), "{:?}", graph.pairs(Calls));
}

#[test]
fn a_call_inside_a_callback_is_made_by_the_enclosing_symbol() {
    let graph = extract(PATH, "function f() {}\nconst handlers = [() => f()];\nconst direct = f();\n");
    assert_edge!(graph, Calls, "handlers", "f");
    assert_edge!(graph, References, "direct", "f");
    assert_no_edge!(graph, Calls, "direct", "f");
}

#[test]
fn a_call_of_something_other_than_a_function_is_a_reference() {
    let graph = extract(PATH, "class C {}\nconst v = 1;\nfunction g() { C(); v(); }\n");
    assert_edge!(graph, References, "g", "C");
    assert_edge!(graph, References, "g", "v");
    assert!(graph.pairs(Calls).is_empty(), "{:?}", graph.pairs(Calls));
}

#[test]
fn a_symbol_never_references_itself() {
    let graph = extract(PATH, "const v = [v];\n");
    assert_no_edge!(graph, References, "v", "v");
}

#[test]
fn a_called_symbol_is_not_also_referenced_by_its_caller() {
    let graph = extract(PATH, "function f() {}\nfunction g() { f(); const h = f; }\n");
    assert_edge!(graph, Calls, "g", "f");
    assert_no_edge!(graph, References, "g", "f");
}

#[test]
fn qualified_and_constructor_calls_resolve_to_members_declared_here() {
    let graph = extract(
        PATH,
        "namespace NS { export function f() {} }\n\
         class K { constructor() {} static s() {} }\n\
         function g() { NS.f(); K.s(); new K(); }\n",
    );
    assert_edge!(graph, Calls, "g", "NS.f");
    assert_edge!(graph, Calls, "g", "K.s");
    assert_edge!(graph, Calls, "g", "K#constructor");
}

#[test]
fn a_variable_receiver_reaches_no_member() {
    let graph = extract(PATH, "const v = 1;\nclass v { m() {} }\nfunction g() { v.m(); }\n");
    assert!(graph.pairs(Calls).is_empty(), "{:?}", graph.pairs(Calls));
}

#[test]
fn this_and_super_calls_resolve_to_members() {
    let graph = extract(
        PATH,
        "class A { m() {} }\n\
         class B extends A { n() { this.o(); super.m(); } o() {} }\n",
    );
    assert_edge!(graph, Calls, "B#n", "B#o");
    assert_edge!(graph, Calls, "B#n", "A#m");
}

#[test]
fn a_bare_name_never_reaches_a_class_member() {
    let graph = extract(PATH, "class Store { pick() {} static put() {} q() { pick(); put(); } }\n");
    assert!(graph.pairs(Calls).is_empty(), "{:?}", graph.pairs(Calls));
}

#[test]
fn a_bare_call_inside_a_method_binds_the_module_function() {
    let graph = extract(PATH, "function pick() {}\nclass Store { pick() { pick(); } }\n");
    assert_edge!(graph, Calls, "Store#pick", "pick");
    assert_no_edge!(graph, Calls, "Store#pick", "Store#pick");
}

#[test]
fn a_require_that_does_not_fold_calls_a_local_require() {
    let graph = extract("src/a.js", "function require(x) {}\nfunction g(p) { require(p); }\n");
    assert_edge!(graph, Calls, "g", "require");
    assert!(graph.pairs(Imports).is_empty(), "{:?}", graph.pairs(Imports));
}

#[test]
fn a_shadowed_const_is_not_folded_into_a_dynamic_import() {
    let shadowed = extract(PATH, "const m = \"./m\";\nfunction g(m) { import(m); }\n");
    assert!(shadowed.pairs(Imports).is_empty(), "{:?}", shadowed.pairs(Imports));
    let visible = extract(PATH, "const m = \"./m\";\nfunction g() { import(m); }\n");
    assert_edge!(visible, Imports, "<file>", "external_module:./m");
}

#[test]
fn a_type_parameter_shadows_only_in_type_position() {
    let graph = extract(
        PATH,
        "class T {}\n\
         function g<T>(x: T) { const y = T; }\n\
         function h<T>(x: T) {}\n",
    );
    assert_edge!(graph, References, "g", "T");
    assert_no_edge!(graph, References, "h", "T");
}

#[test]
fn an_identifier_in_a_binding_position_is_not_a_reference() {
    let graph = extract(PATH, "function key() {}\ninterface I { [key: string]: number }\n");
    assert_no_edge!(graph, References, "I", "key");
}

#[test]
fn jsx_names_and_generic_heads_are_references() {
    let jsx = extract("src/a.tsx", "function C() { return null; }\nfunction g() { return <C />; }\n");
    assert_edge!(jsx, References, "g", "C");
    let generic = extract(PATH, "class Box<T> {}\nfunction g(x: Box<number>) {}\n");
    assert_edge!(generic, References, "g", "Box");
}

// --- heritage ----------------------------------------------------------------

#[test]
fn heritage_names_resolve_to_types_declared_here_from_the_subtype() {
    let graph = extract(
        PATH,
        "class A {}\ninterface I {}\ninterface J extends I {}\nclass B extends A implements I {}\n",
    );
    assert_edge!(graph, SupertypeOf, "B", "A");
    assert_edge!(graph, SupertypeOf, "B", "I");
    assert_edge!(graph, SupertypeOf, "J", "I");
    assert_eq!(graph.pairs(SupertypeOf).len(), 3, "{:?}", graph.pairs(SupertypeOf));
}

#[test]
fn a_heritage_name_declared_elsewhere_resolves_to_its_imports_pending_symbol() {
    let graph = extract_in(
        &[
            ("src/a.ts", "import { Base, I } from \"./base\";\nclass C extends Base implements I {}\n"),
            ("src/base.ts", "export class Base {}\nexport interface I {}\n"),
        ],
        "src/a.ts",
    );
    assert_edge!(graph, SupertypeOf, "C", "pending_symbol:src/base.ts#Base");
    assert_edge!(graph, SupertypeOf, "C", "pending_symbol:src/base.ts#I");
}

#[test]
fn supertypes_come_after_the_late_exports_edges_and_before_calls() {
    let graph =
        extract(PATH, "function f() {}\nclass A {}\nclass B extends A { m() { f(); } }\nexport { A };\n");
    let kinds = graph.kinds();
    let last = |kind: EdgeKind| kinds.iter().rposition(|k| *k == kind).unwrap_or_else(|| panic!("{kinds:?}"));
    let first = |kind: EdgeKind| kinds.iter().position(|k| *k == kind).unwrap_or_else(|| panic!("{kinds:?}"));
    assert!(last(EdgeKind::Exports) < first(SupertypeOf), "{kinds:?}");
    assert!(last(SupertypeOf) < first(Calls), "{kinds:?}");
}

#[test]
fn a_type_declared_inside_a_function_body_records_no_supertype() {
    let graph = extract(PATH, "class A {}\nfunction g() { class B extends A {} }\n");
    assert!(graph.pairs(SupertypeOf).is_empty(), "{:?}", graph.pairs(SupertypeOf));
}
