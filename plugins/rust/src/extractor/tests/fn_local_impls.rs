//! An `impl` block declared as its own
//! node (Decision 8) inside a fn body is qualified by the fns it is inside,
//! as rustc's def paths are, so two fns of one module that each declare a
//! local `Nothing` with its own impl keep two nodes; a block whose name is
//! still taken by another scope of the file is suffixed `#2`, `#3`, ... in
//! source order. Blocks of one name in one scope (such as `#[cfg]`
//! alternatives, which are siblings) are one block, merged as every other
//! item is; so are blocks nested in the bodies of such alternatives, because
//! a scope is a block's position within its named owner, not a byte offset.
//!
//! Its own module, so these tests stay apart from the parent file's; the
//! helpers are the parent's.

use g_mesh_plugin_sdk::wire::{EdgeKind, NodeKind, WireNode};

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

/// The nodes named `qualified_name`, however many there are.
fn nodes_named<'g>(graph: &'g Graph, qualified_name: &str) -> Vec<&'g WireNode> {
    graph.0.nodes.iter().filter(|node| node.qualified_name == qualified_name).collect()
}

/// `#[cfg(a)]` and `#[cfg(not(a))]` alternatives of one fn-local impl block
/// are siblings in one scope, so they are one node, at the first
/// alternative, with one `SUPERTYPE_OF` edge.
///
/// Control: in `Bodies::declare_impl_block`, skip the per-scope lookup and
/// always run the ordinal loop - `g::<L as Tr>#2` appears.
#[test]
fn cfg_alternatives_of_a_fn_local_impl_block_are_one_node() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub trait Tr { fn x(&self); }\n\
         pub fn helper() {}\n\
         pub fn g() {\n    \
         #[cfg(a)] fn inner() {}\n    \
         #[cfg(not(a))] fn inner() {}\n    \
         struct L;\n    \
         #[cfg(a)] impl Tr for L { fn x(&self) { helper(); } }\n    \
         #[cfg(not(a))] impl Tr for L { fn x(&self) { helper(); } }\n\
         }\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(impl_blocks(&graph), vec!["g::<L as Tr>"], "{:#?}", graph.names());
    assert!(graph.0.nodes.iter().all(|node| !node.qualified_name.contains('#')), "{:#?}", graph.names());
    assert_block(&graph, "g::<L as Tr>", "<L as Tr>", "Tr");
    assert_eq!(graph.node("g::<L as Tr>").range.start.line, 6, "the first alternative's line");
    assert_paths_are_well_formed(&graph);
}

/// A module-level Decision-8 block (a trait impl on a tuple) follows the
/// same rule: its cfg alternatives are one block node, agreeing with its
/// method, which is one node too.
///
/// Control: as for the fn-local case - `<(u8, u8) as Tr>#2` appears.
#[test]
fn cfg_alternatives_of_a_module_level_impl_block_are_one_node_like_its_method() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub trait Tr { fn x(&self); }\n\
         #[cfg(a)] pub fn f() {}\n\
         #[cfg(not(a))] pub fn f() {}\n\
         #[cfg(a)] pub struct S;\n\
         #[cfg(not(a))] pub struct S;\n\
         #[cfg(a)] impl S { pub fn m() {} }\n\
         #[cfg(not(a))] impl S { pub fn m() {} }\n\
         #[cfg(a)] impl Tr for S { fn x(&self) {} }\n\
         #[cfg(not(a))] impl Tr for S { fn x(&self) {} }\n\
         #[cfg(a)] impl Tr for (u8, u8) { fn x(&self) {} }\n\
         #[cfg(not(a))] impl Tr for (u8, u8) { fn x(&self) {} }\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(impl_blocks(&graph), vec!["<(u8, u8) as Tr>"], "{:#?}", graph.names());
    assert_block(&graph, "<(u8, u8) as Tr>", "<(u8, u8) as Tr>", "Tr");
    assert_eq!(graph.node("<(u8, u8) as Tr>").range.start.line, 9, "the first alternative's line");
    let methods = nodes_named(&graph, "<(u8, u8) as Tr>::x");
    assert_eq!(methods.len(), 1, "{:#?}", graph.names());
    assert_eq!(methods[0].native_kind.as_deref(), Some("trait_impl_method"));
    assert_paths_are_well_formed(&graph);
}

/// Two `{ }` blocks of one fn, each with its own `K` and two cfg
/// alternatives of `impl Tr for K`: the alternatives of each block merge,
/// and the second block still takes the next ordinal.
///
/// Control: in `Bodies::declare_impl_block`, key the remembered names by
/// the unsuffixed name alone (no scope) - the second block merges into the
/// first and `f::<K as Tr>#2` is missing.
#[test]
fn cfg_alternatives_merge_within_each_scope_and_scopes_keep_their_ordinals() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub trait Tr {}\n\
         pub fn f() {\n    \
         {\n        struct K;\n        \
         #[cfg(a)] impl Tr for K {}\n        \
         #[cfg(not(a))] impl Tr for K {}\n    }\n    \
         {\n        struct K;\n        \
         #[cfg(a)] impl Tr for K {}\n        \
         #[cfg(not(a))] impl Tr for K {}\n    }\n\
         }\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(impl_blocks(&graph), vec!["f::<K as Tr>", "f::<K as Tr>#2"], "{:#?}", graph.names());
    assert_block(&graph, "f::<K as Tr>", "<K as Tr>", "Tr");
    assert_block(&graph, "f::<K as Tr>#2", "<K as Tr>#2", "Tr");
    assert_eq!(graph.node("f::<K as Tr>").range.start.line, 4, "the first scope's first alternative");
    assert_eq!(graph.node("f::<K as Tr>#2").range.start.line, 9, "the second scope's first alternative");
    assert_paths_are_well_formed(&graph);
}

/// Two real impls in one scope whose names match, because a block's name
/// drops the trait's generic arguments (`Tr<u8>`, `Tr<u16>`), are one
/// block, as their methods already are at module level.
///
/// Control: as for the cfg alternatives - `h::<L as Tr>#2` appears.
#[test]
fn same_named_impls_in_one_scope_are_one_block() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub trait Tr<T> { fn x(&self); }\n\
         pub fn h() {\n    \
         struct L;\n    \
         impl Tr<u8> for L { fn x(&self) {} }\n    \
         impl Tr<u16> for L { fn x(&self) {} }\n\
         }\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(impl_blocks(&graph), vec!["h::<L as Tr>"], "{:#?}", graph.names());
    let block = graph.node("h::<L as Tr>");
    assert_eq!(block.name, "<L as Tr>", "{block:#?}");
    assert_eq!(block.range.start.line, 3, "the first impl's line");
    assert!(
        graph.targets(EdgeKind::SupertypeOf, "h::<L as Tr>").iter().all(|target| target == "Tr"),
        "{:#?}",
        graph.edges(EdgeKind::SupertypeOf)
    );
    assert_paths_are_well_formed(&graph);
}

/// Two `{ }` scopes of one fn, each with its own `K`, an `impl Tr for K`
/// and, inside its method, a block of its own: the second scope's block is
/// `<K as Tr>#2`, and the block nested in its method is qualified by that
/// assigned name, so each nested block is one node with no ordinal.
///
/// Control: in `Bodies::function`, always push `member_tail_path(block,
/// name)` - the second nested block reads `f::<K as Tr>::x::<M as U>#2`.
#[test]
fn a_block_nested_in_a_member_of_an_ordinal_block_takes_its_assigned_name() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub trait Tr { fn x(&self); }\n\
         pub trait U { fn y(&self); }\n\
         pub fn f() {\n    \
         { struct K; impl Tr for K { fn x(&self) { struct M; impl U for M { fn y(&self) {} } } } }\n    \
         { struct K; impl Tr for K { fn x(&self) { struct M; impl U for M { fn y(&self) {} } } } }\n\
         }\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(
        impl_blocks(&graph),
        vec!["f::<K as Tr>", "f::<K as Tr>#2", "f::<K as Tr>#2::x::<M as U>", "f::<K as Tr>::x::<M as U>"],
        "{:#?}",
        graph.names()
    );
    assert_block(&graph, "f::<K as Tr>", "<K as Tr>", "Tr");
    assert_block(&graph, "f::<K as Tr>#2", "<K as Tr>#2", "Tr");
    assert_block(&graph, "f::<K as Tr>::x::<M as U>", "<M as U>", "U");
    assert_block(&graph, "f::<K as Tr>#2::x::<M as U>", "<M as U>", "U");
    assert_eq!(graph.node("f::<K as Tr>::x::<M as U>").range.start.line, 3, "the first scope's block");
    assert_eq!(graph.node("f::<K as Tr>#2::x::<M as U>").range.start.line, 4, "the second scope's block");
    assert_paths_are_well_formed(&graph);
}

/// A module-level type with two inherent impl blocks and two trait impls:
/// every method is qualified by its own block, and nothing is suffixed.
#[test]
fn methods_of_each_module_level_impl_block_are_qualified_by_their_block() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub trait Tr { fn x(&self); }\n\
         pub trait U { fn y(&self); }\n\
         pub struct S;\n\
         impl S { pub fn a() {} }\n\
         impl S { pub fn b() {} }\n\
         impl Tr for S { fn x(&self) {} }\n\
         impl U for S { fn y(&self) {} }\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    for method in ["S::a", "S::b", "<S as Tr>::x", "<S as U>::y"] {
        assert_eq!(nodes_named(&graph, method).len(), 1, "{method}: {:#?}", graph.names());
    }
    assert!(graph.0.nodes.iter().all(|node| !node.qualified_name.contains('#')), "{:#?}", graph.names());
    assert_paths_are_well_formed(&graph);
}

/// The traits every GM-691 fixture below implements.
const GM691_TRAITS: &str = "pub trait Tr { fn x(&self); }\npub trait U {}\n";

/// Extracts `GM691_TRAITS` followed by `body` as `src/lib.rs`.
fn extract_gm691(body: &str) -> Graph {
    Crate::new(&[("src/lib.rs", &format!("{GM691_TRAITS}{body}"))]).extract("src/lib.rs")
}

/// A block nested in a method of merged `#[cfg]` alternatives of a
/// fn-local impl is one node, at the first alternative; three levels deep
/// (a block in a method of a block in a method of the merged alternatives)
/// no `#` appears anywhere.
///
/// Control: restore `impl_block_scope` to `vec![parent.start_byte()]` (the
/// byte offset of the enclosing block) - `g::<L as Tr>::x::<M as U>#2` and
/// `f::<K as Tr>::x::<M as Tr>#2` appear.
#[test]
fn a_block_nested_in_a_method_of_merged_cfg_alternatives_is_one_node() {
    let graph = extract_gm691(
        "pub fn g() {\n struct L;\n \
         #[cfg(a)] impl Tr for L { fn x(&self) { struct M; impl U for M {} } }\n \
         #[cfg(not(a))] impl Tr for L { fn x(&self) { struct M; impl U for M {} } }\n}\n",
    );
    assert_eq!(
        impl_blocks(&graph),
        vec!["g::<L as Tr>", "g::<L as Tr>::x::<M as U>"],
        "{:#?}",
        graph.names()
    );
    assert_block(&graph, "g::<L as Tr>::x::<M as U>", "<M as U>", "U");
    assert_eq!(graph.node("g::<L as Tr>::x::<M as U>").range.start.line, 4, "the first alternative's block");
    assert_paths_are_well_formed(&graph);

    let graph = extract_gm691(
        "pub fn f() {\n struct K;\n \
         #[cfg(a)] impl Tr for K { fn x(&self) { struct M; impl Tr for M { fn x(&self) { struct N; impl U for N {} } } } }\n \
         #[cfg(not(a))] impl Tr for K { fn x(&self) { struct M; impl Tr for M { fn x(&self) { struct N; impl U for N {} } } } }\n}\n",
    );
    assert_eq!(
        impl_blocks(&graph),
        vec!["f::<K as Tr>", "f::<K as Tr>::x::<M as Tr>", "f::<K as Tr>::x::<M as Tr>::x::<N as U>"],
        "{:#?}",
        graph.names()
    );
    assert!(graph.0.nodes.iter().all(|node| !node.qualified_name.contains('#')), "{:#?}", graph.names());
    assert_block(&graph, "f::<K as Tr>::x::<M as Tr>::x::<N as U>", "<N as U>", "U");
    assert_paths_are_well_formed(&graph);
}

/// A block in the body of top-level `#[cfg]` alternatives of a fn (or of
/// fn-local ones) is one node, at the first alternative, also when the
/// alternatives differ before it: an extra statement or an extra `{ }`
/// block in one alternative only.
///
/// Control: as above (`impl_block_scope` keyed by the enclosing block's
/// byte offset) - every case gains a `#2`.
#[test]
fn a_block_in_the_body_of_merged_cfg_alternatives_of_a_fn_is_one_node() {
    for (case, body, wanted) in [
        (
            "P2 top-level alternatives",
            "#[cfg(a)] pub fn f() { struct M; impl U for M {} }\n\
             #[cfg(not(a))] pub fn f() { struct M; impl U for M {} }\n",
            "f::<M as U>",
        ),
        (
            "P3 fn-local alternatives",
            "pub fn g() {\n #[cfg(a)] fn h() { struct M; impl U for M {} }\n \
             #[cfg(not(a))] fn h() { struct M; impl U for M {} }\n}\n",
            "g::h::<M as U>",
        ),
        (
            "P8 a statement in one alternative only",
            "#[cfg(a)] pub fn f() { let _a = 1; struct M; impl U for M {} }\n\
             #[cfg(not(a))] pub fn f() { struct M; impl U for M {} }\n",
            "f::<M as U>",
        ),
        (
            "P12 a block in one alternative only",
            "#[cfg(a)] pub fn f() { { let _ = 1; } struct M; impl U for M {} }\n\
             #[cfg(not(a))] pub fn f() { struct M; impl U for M {} }\n",
            "f::<M as U>",
        ),
    ] {
        let graph = extract_gm691(body);
        assert_eq!(impl_blocks(&graph), vec![wanted], "{case}: {:#?}", graph.names());
        assert_block(&graph, wanted, "<M as U>", "U");
        assert_paths_are_well_formed(&graph);
    }
    let graph = extract_gm691(
        "#[cfg(a)] pub fn f() { struct M; impl U for M {} }\n\
         #[cfg(not(a))] pub fn f() { struct M; impl U for M {} }\n",
    );
    assert_eq!(graph.node("f::<M as U>").range.start.line, 2, "the first alternative's block");
}

/// A block in a nested `{ }` or a closure of merged `#[cfg]` alternatives
/// is one node per position: each alternative with two `{ }` blocks gives
/// exactly two nodes (one per block, not one per block per alternative),
/// and a block in a closure, or in one nested block, gives one.
///
/// Control: as above (`impl_block_scope` keyed by the enclosing block's
/// byte offset) - P7 gives `#3`/`#4`, P4 and P9 give `#2`.
#[test]
fn blocks_in_nested_scopes_of_merged_cfg_alternatives_are_one_node_per_position() {
    let graph = extract_gm691(
        "#[cfg(a)] pub fn f() { { struct M; impl U for M {} } { struct M; impl U for M {} } }\n\
         #[cfg(not(a))] pub fn f() { { struct M; impl U for M {} } { struct M; impl U for M {} } }\n",
    );
    assert_eq!(impl_blocks(&graph), vec!["f::<M as U>", "f::<M as U>#2"], "P7: {:#?}", graph.names());
    assert_block(&graph, "f::<M as U>", "<M as U>", "U");
    assert_block(&graph, "f::<M as U>#2", "<M as U>#2", "U");
    assert_paths_are_well_formed(&graph);

    for (case, body) in [
        (
            "P4 one nested block",
            "#[cfg(a)] pub fn f() { { struct M; impl U for M {} } }\n\
             #[cfg(not(a))] pub fn f() { { struct M; impl U for M {} } }\n",
        ),
        (
            "P9 a closure",
            "#[cfg(a)] pub fn f() { let _c = || { struct M; impl U for M {} }; }\n\
             #[cfg(not(a))] pub fn f() { let _c = || { struct M; impl U for M {} }; }\n",
        ),
    ] {
        let graph = extract_gm691(body);
        assert_eq!(impl_blocks(&graph), vec!["f::<M as U>"], "{case}: {:#?}", graph.names());
        assert_block(&graph, "f::<M as U>", "<M as U>", "U");
        assert_paths_are_well_formed(&graph);
    }
}

/// Blocks in genuinely different anonymous scopes of one owner keep their
/// ordinals: same-named fns in two `{ }` blocks of one fn, fns in two
/// `const _` initializers, two sibling `{ }` blocks, the branches of an
/// `if`/`else`, and the initializers of two named consts (whose names the
/// qualified name does not carry).
///
/// Control: make `block_ordinal` return 0 - every pair merges and its `#2`
/// is missing.
#[test]
fn blocks_in_different_anonymous_scopes_of_one_owner_keep_their_ordinals() {
    for (case, body, first, second) in [
        (
            "P5 same-named fns in two blocks",
            "pub fn g() {\n { fn h() { struct M; impl U for M {} } }\n \
             { fn h() { struct M; impl U for M {} } }\n}\n",
            "g::h::<M as U>",
            "g::h::<M as U>#2",
        ),
        (
            "P6 fns in two const _",
            "const _: () = { fn h() { struct M; impl U for M {} } };\n\
             const _: () = { fn h() { struct M; impl U for M {} } };\n",
            "h::<M as U>",
            "h::<M as U>#2",
        ),
        (
            "P11 sibling blocks",
            "pub fn f() {\n { struct M; impl U for M {} }\n { struct M; impl U for M {} }\n}\n",
            "f::<M as U>",
            "f::<M as U>#2",
        ),
        (
            "P13 if/else branches",
            "pub fn f(c: bool) {\n if c { struct M; impl U for M {} } else { struct M; impl U for M {} }\n}\n",
            "f::<M as U>",
            "f::<M as U>#2",
        ),
        (
            "P15 two named consts",
            "const C: () = { struct M; impl U for M {} };\n\
             const D: () = { struct M; impl U for M {} };\n",
            "<M as U>",
            "<M as U>#2",
        ),
    ] {
        let graph = extract_gm691(body);
        assert_eq!(impl_blocks(&graph), vec![first, second], "{case}: {:#?}", graph.names());
        assert_block(&graph, first, "<M as U>", "U");
        assert_block(&graph, second, "<M as U>#2", "U");
        assert_paths_are_well_formed(&graph);
    }
}

/// Documented residuals (GM-691 design note, section 5): `#[cfg]`
/// alternatives of a named const are module-level blocks, not owners, so
/// their initializers are two scopes; and alternatives whose block
/// structure differs before a nested-block impl shift its ordinal. Both
/// stay split into `#2`.
#[test]
fn cfg_alternatives_of_a_named_const_or_of_differing_block_structure_stay_split_as_documented() {
    for (case, body, wanted) in [
        (
            "P14 named const alternatives",
            "#[cfg(a)] const C: () = { struct M; impl U for M {} };\n\
             #[cfg(not(a))] const C: () = { struct M; impl U for M {} };\n",
            ["<M as U>", "<M as U>#2"],
        ),
        (
            "P16 an extra block before a nested-block impl",
            "#[cfg(a)] pub fn f() { { let _ = 1; } { struct M; impl U for M {} } }\n\
             #[cfg(not(a))] pub fn f() { { struct M; impl U for M {} } }\n",
            ["f::<M as U>", "f::<M as U>#2"],
        ),
    ] {
        let graph = extract_gm691(body);
        assert_eq!(impl_blocks(&graph), wanted, "{case}: {:#?}", graph.names());
        assert_paths_are_well_formed(&graph);
    }
}
