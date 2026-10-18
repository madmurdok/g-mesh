//! GM-630 / ADR 0038: an enum variant is a node of its own, named `E::V`,
//! of the kind its shape takes in Rust's value namespace.
//!
//! Its own module, so these tests stay apart from the open-site tests in
//! the parent file; the helpers are the parent's.

use g_mesh_plugin_sdk::wire::{EdgeKind, NodeKind, Visibility};

use super::Crate;

/// Behaviour 1: one node per variant, by shape - a tuple variant is a
/// `Function` (a constructor), a unit variant a `Variable`, a struct variant
/// a `Type` - all `nativeKind` `variant`, placed in the enum's module and as
/// visible as the enum, carrying their own signature and doc comment.
///
/// The file's declarations are asserted as an exact set, so a variant node
/// gained or lost (and every Rust node count with it) changes this test.
#[test]
fn every_variant_is_a_node_of_the_kind_its_shape_gives() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub enum E {
    /// The unit one.
    U,
    T(u8),
    S { a: u8 },
}
enum P { Hidden }
pub mod inner {
    pub enum G { V(u8) }
}
"#,
    )]);
    let graph = krate.extract("src/lib.rs");

    let mut declarations: Vec<(String, NodeKind, String)> = graph
        .0
        .nodes
        .iter()
        .filter(|node| node.kind != NodeKind::File)
        .filter(|node| !node.native_kind.as_deref().is_some_and(|native| native.ends_with("symbol")))
        .map(|node| (node.qualified_name.clone(), node.kind, node.native_kind.clone().unwrap_or_default()))
        .collect();
    declarations.sort_by(|a, b| a.0.cmp(&b.0));
    let expected: Vec<(String, NodeKind, String)> = [
        ("E", NodeKind::Type, "enum"),
        ("E::S", NodeKind::Type, "variant"),
        ("E::T", NodeKind::Function, "variant"),
        ("E::U", NodeKind::Variable, "variant"),
        ("P", NodeKind::Type, "enum"),
        ("P::Hidden", NodeKind::Variable, "variant"),
        ("inner", NodeKind::Module, "module"),
        ("inner::G", NodeKind::Type, "enum"),
        ("inner::G::V", NodeKind::Function, "variant"),
    ]
    .into_iter()
    .map(|(name, kind, native)| (name.to_string(), kind, native.to_string()))
    .collect();
    assert_eq!(declarations, expected, "{:#?}", graph.names());

    for qualified_name in ["E::U", "E::T", "E::S"] {
        let node = graph.node(qualified_name);
        assert_eq!(node.name, qualified_name.rsplit("::").next().unwrap(), "{qualified_name}");
        assert_eq!(node.container.as_deref(), Some("krate"), "{qualified_name}");
        assert_eq!(node.visibility, Visibility::Public, "{qualified_name}: as visible as `pub enum E`");
    }
    assert_eq!(graph.node("P::Hidden").visibility, graph.node("P").visibility, "as visible as `enum P`");
    assert_ne!(graph.node("P::Hidden").visibility, Visibility::Public);
    assert_eq!(graph.node("inner::G::V").container.as_deref(), Some("krate::inner"));

    assert_eq!(graph.node("E::T").signature.as_deref(), Some("T(u8)"));
    assert_eq!(graph.node("E::S").signature.as_deref(), Some("S { a: u8 }"));
    assert!(
        graph.node("E::U").doc_comment.as_deref().is_some_and(|doc| doc.contains("The unit one.")),
        "{:?}",
        graph.node("E::U").doc_comment
    );
}

/// Behaviours 2 and 5 (plugin half): inside one file `E::T(..)` and
/// `Self::T(..)` are resolved `CALLS` onto the tuple variant, `E::U` and
/// `E::S { .. }` resolved `REFERENCES` onto theirs - and no construction is
/// a `CALLS` onto the enum.
#[test]
fn a_same_file_construction_resolves_onto_the_variant_not_the_enum() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub enum E { U, T(u8), S { a: u8 } }
impl E {
    pub fn own() -> E { Self::T(0) }
}
pub fn call() -> E { E::T(1) }
pub fn unit() -> E { E::U }
pub fn record() -> E { E::S { a: 2 } }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");

    assert_eq!(graph.targets(EdgeKind::Calls, "call"), vec!["E::T".to_string()]);
    assert_eq!(graph.targets(EdgeKind::Calls, "E::own"), vec!["E::T".to_string()]);
    assert!(graph.targets(EdgeKind::References, "unit").contains(&"E::U".to_string()));
    assert!(graph.targets(EdgeKind::References, "record").contains(&"E::S".to_string()));
    for from in ["call", "E::own", "unit", "record"] {
        assert!(
            !graph.targets(EdgeKind::Calls, from).contains(&"E".to_string()),
            "{from}: a CALLS onto the enum"
        );
    }
}

/// Behaviour 4: a variant is registered by its `E::V` tail only. A bare `T()`
/// beside `enum E { T(u8) }` stays on the free fn `T`, and a bare `W(..)`
/// that nothing imports is not the variant `H::W`.
#[test]
fn a_bare_name_never_resolves_to_a_variant() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
#![allow(non_snake_case)]
pub enum E { T(u8) }
pub fn T() {}
pub fn bare() { T(); }
pub enum H { W(u8) }
pub fn unimported() -> H { W(1) }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");

    assert_eq!(graph.targets(EdgeKind::Calls, "bare"), vec!["T".to_string()]);
    let unimported = graph.targets(EdgeKind::Calls, "unimported");
    assert!(!unimported.contains(&"H::W".to_string()), "{unimported:?}");
    assert!(graph.find("E::T").is_some() && graph.find("H::W").is_some(), "{:#?}", graph.names());
}

/// Behaviour 6: the types inside a variant's payload are referenced from the
/// enum, never from the variant's own node.
#[test]
fn a_variants_payload_is_referenced_from_its_enum() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub struct Foo;\npub struct Bar;\npub enum E { T(Foo), S { b: Bar } }\n",
    )]);
    let graph = krate.extract("src/lib.rs");

    let from_enum = graph.targets(EdgeKind::References, "E");
    assert!(
        from_enum.contains(&"Foo".to_string()) && from_enum.contains(&"Bar".to_string()),
        "{from_enum:?}"
    );
    for variant in ["E::T", "E::S"] {
        assert_eq!(graph.targets(EdgeKind::References, variant), Vec::<String>::new(), "{variant}");
    }
}

/// Behaviour 7: `cfg` alternatives of one `E::V` are one node, the first
/// declared, whatever the later ones' shapes - in one enum, and across `cfg`
/// copies of the enum.
#[test]
fn cfg_alternatives_of_one_variant_are_one_node_the_first_declared() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub enum C {
    #[cfg(feature = "x")]
    X(u8),
    #[cfg(not(feature = "x"))]
    X,
}
#[cfg(feature = "y")]
pub enum D { Y(u8) }
#[cfg(not(feature = "y"))]
pub enum D { Y }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");

    for qualified_name in ["C::X", "D::Y"] {
        let nodes: Vec<NodeKind> = graph
            .0
            .nodes
            .iter()
            .filter(|node| node.qualified_name == qualified_name)
            .map(|node| node.kind)
            .collect();
        assert_eq!(nodes, vec![NodeKind::Function], "{qualified_name}: one node, the first alternative's");
    }
}
