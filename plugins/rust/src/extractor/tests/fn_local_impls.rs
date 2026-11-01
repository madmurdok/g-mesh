//! An `impl` block declared as its own
//! node (Decision 8) inside a fn body is qualified by the fns it is inside,
//! as rustc's def paths are, so two fns of one module that each declare a
//! local `Nothing` with its own impl keep two nodes; a block whose name is
//! still taken in the file is suffixed `#2`, `#3`, ... in source order.
//!
//! Its own module, so these tests stay apart from the parent file's; the
//! helpers are the parent's.

use g_mesh_plugin_sdk::wire::{EdgeKind, NodeKind};

use super::{assert_paths_are_well_formed, segments, Crate, Graph};

/// The `qualifiedName`s of every `nativeKind = "impl"` block node, sorted.
fn impl_blocks(graph: &Graph) -> Vec<String> {
    let mut blocks: Vec<String> = graph
        .0
        .nodes
        .iter()
        .filter(|node| node.native_kind.as_deref() == Some("impl"))
        .map(|node| node.qualified_name.clone())
        .collect();
    blocks.sort();
    blocks
}

/// Asserts the block `qualified_name` is a `Type` named `name` with one
/// `SUPERTYPE_OF` edge, onto `supertype`.
fn assert_block(graph: &Graph, qualified_name: &str, name: &str, supertype: &str) {
    let block = graph.node(qualified_name);
    assert_eq!(block.kind, NodeKind::Type, "{block:#?}");
    assert_eq!(block.name, name, "{block:#?}");
    assert_eq!(graph.targets(EdgeKind::SupertypeOf, qualified_name), vec![supertype.to_string()]);
}

/// Behaviour 10: `first()` and `second()` of one module each declare a
/// local `Nothing` implementing `Extractor`: two nodes, each named
/// `<Nothing as Extractor>` and qualified by its fn, each with its own
/// `SUPERTYPE_OF` edge; a third, uniquely named local type and a same-named
/// one in another module are unaffected.
///
/// Control: make `Bodies::fn_local_path` return `QualifiedPath::root(name)`:
/// the two `Nothing` blocks collide (the second becomes `#2`, and the
/// `m::first::`/`m::second::` names are missing).
#[test]
fn two_same_named_fn_local_impls_in_one_module_are_two_nodes() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub mod m {\n\
         pub trait Extractor { fn run(&self) -> u32; }\n\
         pub fn first() -> u32 { struct Nothing; impl Extractor for Nothing { fn run(&self) -> u32 { 1 } } Nothing.run() }\n\
         pub fn second() -> u32 { struct Nothing; impl Extractor for Nothing { fn run(&self) -> u32 { 2 } } Nothing.run() }\n\
         pub fn third() -> u32 { struct Other; impl Extractor for Other { fn run(&self) -> u32 { 3 } } Other.run() }\n\
         pub mod elsewhere {\n    use super::Extractor;\n    \
         pub fn fourth() -> u32 { struct Nothing; impl Extractor for Nothing { fn run(&self) -> u32 { 4 } } Nothing.run() }\n}\n\
         }\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(
        impl_blocks(&graph),
        vec![
            "m::elsewhere::fourth::<Nothing as Extractor>",
            "m::first::<Nothing as Extractor>",
            "m::second::<Nothing as Extractor>",
            "m::third::<Other as Extractor>",
        ],
        "{:#?}",
        graph.names()
    );
    let first = graph.node("m::first::<Nothing as Extractor>");
    let second = graph.node("m::second::<Nothing as Extractor>");
    assert_ne!(first.id, second.id);
    assert_block(&graph, "m::first::<Nothing as Extractor>", "<Nothing as Extractor>", "m::Extractor");
    assert_block(&graph, "m::second::<Nothing as Extractor>", "<Nothing as Extractor>", "m::Extractor");
    assert_eq!(
        segments(first.qualified_path.as_ref().expect("a declaration carries its path")),
        vec![
            (String::new(), "m".to_string()),
            ("::".to_string(), "first".to_string()),
            ("::".to_string(), "<Nothing as Extractor>".to_string()),
        ]
    );
    assert_eq!(first.container.as_deref(), Some("krate::m"), "still a member of its module");
    assert_paths_are_well_formed(&graph);
}

/// Behaviour 11: inside an inherent method the block is `T::m::<..>`,
/// inside a trait-impl method `<T as X>::x::<..>`; nested fns stack
/// outermost first, and a fn nested in a method body (not a member of the
/// impl) adds its bare name after the method's path.
///
/// Control: in `Bodies::function`, push the bare name for members too
/// (`member_tail_path(None, name)`) - `T::m::` becomes `m::`.
#[test]
fn a_fn_local_impl_is_qualified_by_every_enclosing_fn_and_method() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub trait Tr {}\npub trait X { fn x(); }\npub struct T;\n\
         impl T {\n    pub fn m() { struct L; impl Tr for L {} }\n    \
         pub fn n() { fn h() { struct L; impl Tr for L {} } }\n}\n\
         impl X for T { fn x() { struct L; impl Tr for L {} } }\n\
         pub fn f() { fn g() { struct L; impl Tr for L {} } }\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(
        impl_blocks(&graph),
        vec!["<T as X>::x::<L as Tr>", "T::m::<L as Tr>", "T::n::h::<L as Tr>", "f::g::<L as Tr>"],
        "{:#?}",
        graph.names()
    );
    for block in impl_blocks(&graph) {
        assert_block(&graph, &block, "<L as Tr>", "Tr");
    }
    assert_paths_are_well_formed(&graph);
}

/// F2: two `const _: () = { struct K; impl Tr for K {} };` items of one
/// module, or three `{ .. }` blocks of one fn, which rustc tells apart and
/// a path cannot: the later ones are `#2`, `#3`, each its own node with its
/// own `SUPERTYPE_OF` edge, rather than dropped by the emitter's
/// first-wins rule. A block whose name is free keeps it plain.
///
/// Control: in `Bodies::declare_impl_block`, drop the `is_declared` loop -
/// the `#2`/`#3` nodes are missing.
#[test]
fn a_colliding_impl_block_is_suffixed_and_keeps_its_edge() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub trait Tr {}\n\
         const _: () = { struct K; impl Tr for K {} };\n\
         const _: () = { struct K; impl Tr for K {} };\n\
         pub fn f() {\n    { struct K; impl Tr for K {} }\n    { struct K; impl Tr for K {} }\n    \
         { struct K; impl Tr for K {} }\n    { struct J; impl Tr for J {} }\n}\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(
        impl_blocks(&graph),
        vec!["<K as Tr>", "<K as Tr>#2", "f::<J as Tr>", "f::<K as Tr>", "f::<K as Tr>#2", "f::<K as Tr>#3"],
        "{:#?}",
        graph.names()
    );
    for (block, name) in [
        ("<K as Tr>", "<K as Tr>"),
        ("<K as Tr>#2", "<K as Tr>#2"),
        ("f::<K as Tr>", "<K as Tr>"),
        ("f::<K as Tr>#2", "<K as Tr>#2"),
        ("f::<K as Tr>#3", "<K as Tr>#3"),
        ("f::<J as Tr>", "<J as Tr>"),
    ] {
        assert_block(&graph, block, name, "Tr");
    }
    assert_paths_are_well_formed(&graph);
}
