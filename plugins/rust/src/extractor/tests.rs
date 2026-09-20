//! Unit tests for the extractor, one per shape the task names.
//!
//! Every one of them runs against a **real** [`ProjectContext`] built from a
//! real (temporary) crate on disk, rather than against
//! `ProjectContext::default()`. That is not thoroughness for its own sake:
//! the default model places every file as an *orphan*, whose container key is
//! `orphan:<path>` and whose crate root is itself - so `crate::`,
//! `pub(crate)` and every cross-module address would be tested in the one
//! configuration where they are all degenerate, and a test suite that passed
//! would say nothing about the case the plugin actually runs in.

use std::path::PathBuf;

use g_mesh_plugin_sdk::wire::{EdgeKind, NodeKind, TargetKey, TargetScope, Visibility, WireEdge, WireNode};
use g_mesh_plugin_sdk::{Extractor, FileGraph, OpenSiteKind, RelPath};

use super::RustExtractor;
use crate::project::ProjectContext;

/// A temporary crate, removed on drop. The same shape `crate::project`'s own
/// tests use, and unique per call for the same reason: `cargo test` runs
/// these concurrently in one process.
struct Crate {
    root: PathBuf,
    project: ProjectContext,
}

impl Crate {
    /// A single package named `krate` whose files are `files`, each a
    /// `(path, contents)` pair. The first file is conventionally
    /// `src/lib.rs`.
    fn new(files: &[(&str, &str)]) -> Self {
        // A process-wide counter, not a timestamp: `cargo test` runs these
        // concurrently in one process, and two trees that share a path race
        // on `create_dir_all`/`remove_dir_all` - which shows up as a *later*
        // test seeing a crate root that another test has just deleted, i.e.
        // as an assertion about containers failing for no reason connected to
        // containers.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("g-mesh-plugin-rust-extract-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"krate\"\nversion = \"0.1.0\"\n")
            .unwrap();
        for (path, contents) in files {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, contents).unwrap();
        }
        let project = ProjectContext::load(&root).unwrap();
        Self { root, project }
    }

    fn extract(&self, path: &str) -> Graph {
        let source = std::fs::read_to_string(self.root.join(path)).unwrap();
        Graph(RustExtractor.extract(&self.project, &RelPath::new(path), &source))
    }
}

impl Drop for Crate {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// One file's graph, with the questions the tests ask of it.
struct Graph(FileGraph);

impl Graph {
    fn node(&self, qualified_name: &str) -> &WireNode {
        self.0
            .nodes
            .iter()
            .find(|node| node.qualified_name == qualified_name)
            .unwrap_or_else(|| panic!("no node {qualified_name:?} in {:#?}", self.names()))
    }

    fn find(&self, qualified_name: &str) -> Option<&WireNode> {
        self.0.nodes.iter().find(|node| node.qualified_name == qualified_name)
    }

    fn by_id(&self, id: &str) -> &WireNode {
        self.0.nodes.iter().find(|node| node.id == id).expect("every edge names a node of this file")
    }

    /// Every node's `(qualifiedName, nativeKind)`, for a failure message that
    /// says what the extractor actually produced.
    fn names(&self) -> Vec<(String, Option<String>)> {
        self.0.nodes.iter().map(|n| (n.qualified_name.clone(), n.native_kind.clone())).collect()
    }

    fn edges(&self, kind: EdgeKind) -> Vec<&WireEdge> {
        self.0.edges.iter().filter(|edge| edge.kind == kind).collect()
    }

    /// The nodes an edge of `kind` out of `from` lands on, rendered as
    /// `qualifiedName` (a declaration) or `nativeKind qualifiedName` (a
    /// placeholder, whose qualified name is the address it is waiting on).
    fn targets(&self, kind: EdgeKind, from: &str) -> Vec<String> {
        let from = self.node(from).id.clone();
        let mut targets: Vec<String> = self
            .edges(kind)
            .into_iter()
            .filter(|edge| edge.from_id == from)
            .map(|edge| {
                let node = self.by_id(&edge.to_id);
                match &node.native_kind {
                    Some(native) if native.ends_with("symbol") || native.ends_with("module") => {
                        format!("{native} {}", node.qualified_name)
                    }
                    _ => node.qualified_name.clone(),
                }
            })
            .collect();
        targets.sort();
        targets
    }

    fn placeholder(&self, native_kind: &str, name: &str) -> &WireNode {
        self.0
            .nodes
            .iter()
            .find(|node| node.native_kind.as_deref() == Some(native_kind) && node.name == name)
            .unwrap_or_else(|| panic!("no {native_kind} named {name:?} in {:#?}", self.names()))
    }

    fn target_of(&self, node: &WireNode) -> (TargetScope, TargetKey) {
        let target = node.target.clone().expect("a linkable placeholder carries its target");
        (target.scope, target.key)
    }
}

fn container(key: &str) -> TargetScope {
    TargetScope::Container(key.to_string())
}

// --- declarations -------------------------------------------------------------

#[test]
fn every_item_kind_becomes_the_node_the_design_doc_names() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub fn free() {}
pub struct S;
pub enum E { A }
pub union U { a: u8 }
pub type Alias = u8;
pub trait Tr { fn required(&self); }
pub const C: u8 = 1;
pub static ST: u8 = 2;
macro_rules! mac { () => {}; }
impl S { pub fn inherent(&self) {} }
impl Tr for S { fn required(&self) {} }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    let expected = [
        ("free", NodeKind::Function, "function"),
        ("S", NodeKind::Type, "struct"),
        ("E", NodeKind::Type, "enum"),
        ("U", NodeKind::Type, "union"),
        ("Alias", NodeKind::Type, "type_alias"),
        ("Tr", NodeKind::Type, "trait"),
        ("Tr::required", NodeKind::Function, "trait_method"),
        ("C", NodeKind::Variable, "const"),
        ("ST", NodeKind::Variable, "static"),
        ("mac", NodeKind::Function, "macro"),
        ("S::inherent", NodeKind::Function, "method"),
        ("<S as Tr>::required", NodeKind::Function, "trait_impl_method"),
    ];
    for (qualified_name, kind, native_kind) in expected {
        let node = graph.node(qualified_name);
        assert_eq!(node.kind, kind, "{qualified_name}");
        assert_eq!(node.native_kind.as_deref(), Some(native_kind), "{qualified_name}");
        assert_eq!(node.container.as_deref(), Some("krate"), "{qualified_name}");
    }
}

/// Decision 2: two impls of two traits on one type keep their own nodes. With
/// the design doc's sketched `T::m`, both would be one id and one of them
/// would be lost.
#[test]
fn two_traits_implemented_on_one_type_keep_their_same_named_methods_apart() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub struct P;\npub trait A { fn go(&self); }\npub trait B { fn go(&self); }\n\
         impl A for P { fn go(&self) {} }\nimpl B for P { fn go(&self) {} }\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    let a = graph.node("<P as A>::go");
    let b = graph.node("<P as B>::go");
    assert_ne!(a.id, b.id);
    assert_eq!(a.native_kind.as_deref(), Some("trait_impl_method"));
    assert_eq!(b.native_kind.as_deref(), Some("trait_impl_method"));
    assert_eq!(a.name, "go", "the bare name is still what a `use` looks up");
}

/// Decision 2: the module path is in the `qualifiedName`, which is what keeps
/// two inline modules' same-named items two nodes rather than one.
#[test]
fn inline_modules_namespace_their_items() {
    let krate = Crate::new(&[("src/lib.rs", "mod a { pub fn helper() {} }\nmod b { pub fn helper() {} }\n")]);
    let graph = krate.extract("src/lib.rs");
    let first = graph.node("a::helper");
    let second = graph.node("b::helper");
    assert_ne!(first.id, second.id);
    assert_eq!(first.container.as_deref(), Some("krate::a"));
    assert_eq!(second.container.as_deref(), Some("krate::b"));
    assert_eq!(first.container_parent.as_deref(), Some("krate"));
}

/// The handoff's requirement: a `mod` item is a member of the module that
/// *declares* it, so `graph::containers::parent_chain` has no gap to stop at.
#[test]
fn a_mod_item_is_a_member_of_the_module_that_declares_it() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod outer;\n"),
        ("src/outer/mod.rs", "pub mod inner;\n"),
        ("src/outer/inner.rs", "pub fn leaf() {}\n"),
    ]);

    let root = krate.extract("src/lib.rs");
    let outer = root.node("outer");
    assert_eq!(outer.kind, NodeKind::Module);
    assert_eq!(outer.native_kind.as_deref(), Some("module"));
    assert_eq!(outer.container.as_deref(), Some("krate"), "a member of the module that declares it");
    assert_eq!(outer.container_parent, None, "`krate` is a crate root");

    let middle = krate.extract("src/outer/mod.rs");
    let inner = middle.node("outer::inner");
    assert_eq!(inner.container.as_deref(), Some("krate::outer"));
    assert_eq!(inner.container_parent.as_deref(), Some("krate"));

    let leaf = krate.extract("src/outer/inner.rs");
    let function = leaf.node("outer::inner::leaf");
    assert_eq!(function.container.as_deref(), Some("krate::outer::inner"));
    assert_eq!(function.container_parent.as_deref(), Some("krate::outer"));
}

/// The `File` node is the one node that must *not* carry a container:
/// `graph::containers` counts any node that does as a member.
#[test]
fn the_file_node_carries_the_module_doc_and_no_container() {
    let krate = Crate::new(&[("src/lib.rs", "//! What this module is for.\npub fn f() {}\n")]);
    let graph = krate.extract("src/lib.rs");
    let file = graph.node("src/lib.rs");
    assert_eq!(file.kind, NodeKind::File);
    assert_eq!(file.container, None);
    assert_eq!(file.doc_comment.as_deref(), Some("What this module is for."));
    assert!(graph.0.nodes.first().is_some_and(|node| node.id == file.id), "the File node comes first");
}

#[test]
fn a_doc_comment_and_a_signature_are_carried_on_the_declaration() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "/// Adds two numbers.\n/// Twice.\n#[inline]\npub fn add(\n    a: u8,\n    b: u8,\n) -> u8 { a + b }\n",
    )]);
    let node = krate.extract("src/lib.rs").node("add").clone();
    assert_eq!(node.doc_comment.as_deref(), Some("Adds two numbers.\nTwice."));
    assert_eq!(node.signature.as_deref(), Some("pub fn add( a: u8, b: u8, ) -> u8"));
}

// --- visibility ---------------------------------------------------------------

#[test]
fn every_visibility_form_maps_to_cores_model() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod a;\n"),
        ("src/a/mod.rs", "pub mod b;\n"),
        (
            "src/a/b.rs",
            "pub fn public() {}\npub(crate) fn crate_wide() {}\npub(super) fn to_parent() {}\n\
             pub(in crate::a) fn to_named() {}\npub(self) fn to_self() {}\nfn private() {}\n",
        ),
    ]);
    let graph = krate.extract("src/a/b.rs");
    let visibility = |name: &str| graph.node(name).visibility.clone();
    assert_eq!(visibility("a::b::public"), Visibility::Public);
    assert_eq!(visibility("a::b::crate_wide"), Visibility::Container("krate".into()));
    assert_eq!(visibility("a::b::to_parent"), Visibility::Container("krate::a".into()));
    assert_eq!(visibility("a::b::to_named"), Visibility::Container("krate::a".into()));
    assert_eq!(visibility("a::b::to_self"), Visibility::Container("krate::a::b".into()));
    assert_eq!(visibility("a::b::private"), Visibility::Container("krate::a::b".into()));
}

#[test]
fn a_traits_items_are_as_visible_as_the_trait_and_a_trait_impls_are_public() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub struct S;\npub(crate) trait Tr { fn m(&self); }\nimpl Tr for S { fn m(&self) {} }\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(graph.node("Tr::m").visibility, Visibility::Container("krate".into()));
    assert_eq!(graph.node("<S as Tr>::m").visibility, Visibility::Public);
}

#[test]
fn a_macro_export_attribute_publishes_a_macro_rules() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "#[macro_export]\nmacro_rules! shouted { () => {}; }\nmacro_rules! quiet { () => {}; }\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(graph.node("shouted").visibility, Visibility::Public);
    assert_eq!(graph.node("quiet").visibility, Visibility::Container("krate".into()));
}

// --- use -----------------------------------------------------------------------

#[test]
fn a_named_use_is_a_name_placeholder_in_the_container_its_path_resolves_to() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod a;\npub mod b;\n"),
        ("src/a.rs", "pub fn f() {}\n"),
        ("src/b.rs", "use crate::a::f;\nuse crate::a::f as g;\npub fn call() { f(); g(); }\n"),
    ]);
    let graph = krate.extract("src/b.rs");
    let placeholder = graph.placeholder("pending_symbol", "f");
    assert_eq!(
        graph.target_of(placeholder),
        (container("krate::a"), TargetKey::Name("f".into())),
        "the address is the container, keyed by the bare name"
    );
    assert_eq!(
        placeholder.target.as_ref().and_then(|t| t.from_container.clone()).as_deref(),
        Some("krate::b"),
        "the requester's own module, for core's visibility check"
    );
    // The alias resolves to the same declaration, so it is the same address
    // and therefore the same placeholder - and both calls hang on it.
    assert_eq!(
        graph.targets(EdgeKind::Calls, "b::call"),
        vec!["pending_symbol krate::a::f"],
        "one address, whatever it is called locally"
    );
}

#[test]
fn a_glob_use_is_a_container_import_and_names_nothing() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod a;\npub mod b;\n"),
        ("src/a.rs", "pub fn f() {}\n"),
        ("src/b.rs", "use crate::a::*;\n"),
    ]);
    let graph = krate.extract("src/b.rs");
    let import = graph.placeholder("resolved_module", "a");
    assert_eq!(graph.target_of(import), (container("krate::a"), TargetKey::Name("*".into())));
    assert_eq!(graph.targets(EdgeKind::Imports, "src/b.rs"), vec!["resolved_module krate::a::*"]);
    assert!(graph.find("krate::a::f").is_none(), "a glob names nothing in particular: {:#?}", graph.names());
}

#[test]
fn a_named_use_also_imports_the_container_it_reads_from() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod a;\npub mod b;\n"),
        ("src/a.rs", "pub fn f() {}\n"),
        ("src/b.rs", "use crate::a::f;\n"),
    ]);
    let graph = krate.extract("src/b.rs");
    assert_eq!(
        graph.targets(EdgeKind::Imports, "src/b.rs"),
        vec!["resolved_module krate::a::*"],
        "`get_dependencies` is answered from IMPORTS edges, and a `use` is an import"
    );
}

#[test]
fn a_pub_use_is_a_reexport_carrying_its_published_name_and_its_container() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod a;\npub mod b;\n"),
        ("src/a.rs", "pub fn f() {}\n"),
        ("src/b.rs", "pub use crate::a::f as renamed;\npub use crate::a::*;\n"),
    ]);
    let graph = krate.extract("src/b.rs");
    let named = graph.placeholder("reexport", "renamed");
    assert_eq!(
        graph.target_of(named),
        (container("krate::a"), TargetKey::Name("f".into())),
        "published as `renamed`, and really `a`'s `f`"
    );
    assert_eq!(
        named.container.as_deref(),
        Some("krate::b"),
        "a container scope's re-exports are the ones whose node sits in that container"
    );
    let glob = graph.placeholder("reexport", "*");
    assert_eq!(graph.target_of(glob), (container("krate::a"), TargetKey::Name("*".into())));
}

#[test]
fn a_use_of_a_crate_this_project_does_not_model_is_an_external_module() {
    let krate = Crate::new(&[("src/lib.rs", "use serde::Serialize;\npub fn f() { Serialize::go(); }\n")]);
    let graph = krate.extract("src/lib.rs");
    let external = graph.placeholder("external_module", "serde");
    assert!(external.target.is_none(), "core never links an external module, so it carries no address");
    assert_eq!(graph.targets(EdgeKind::Imports, "src/lib.rs"), vec!["external_module serde"]);
    assert!(
        graph.targets(EdgeKind::Calls, "f").is_empty(),
        "a name known to come from another crate is not addressed at this project's own modules"
    );
}

#[test]
fn a_nested_use_group_places_every_leaf_in_its_own_container() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod a;\npub mod b;\n"),
        ("src/a.rs", "pub mod deep;\npub fn f() {}\n"),
        ("src/a/deep.rs", "pub fn g() {}\n"),
        ("src/b.rs", "use crate::a::{f, deep::g};\n"),
    ]);
    let graph = krate.extract("src/b.rs");
    assert_eq!(graph.target_of(graph.placeholder("pending_symbol", "f")).0, container("krate::a"));
    assert_eq!(graph.target_of(graph.placeholder("pending_symbol", "g")).0, container("krate::a::deep"));
}

/// GM-358, the Rust shape of the same gap the Python plugin had:
/// `use crate::a::b;`'s leaf, `b`, is a real (file-backed) submodule of `a`
/// rather than a symbol declared inside it, so the `use` also loads `a::b`
/// as a side effect. `f` (an ordinary symbol, resolved through `a`'s own
/// container edge exactly as before this fix) is the control: it must not
/// gain a phantom edge onto `krate::a::f`, which shows the fix discriminates
/// rather than firing on every named `use` indiscriminately.
#[test]
fn a_use_of_a_submodule_gains_an_imports_edge_a_plain_symbol_use_does_not() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod a;\npub mod c;\n"),
        ("src/a.rs", "pub mod b;\npub fn f() {}\n"),
        ("src/a/b.rs", "pub fn g() {}\n"),
        ("src/c.rs", "use crate::a::b;\nuse crate::a::f;\npub fn run() { b::g(); f(); }\n"),
    ]);
    let graph = krate.extract("src/c.rs");
    assert_eq!(
        graph.targets(EdgeKind::Imports, "src/c.rs"),
        vec!["resolved_module krate::a::*".to_string(), "resolved_module krate::a::b::*".to_string()],
        "{:#?}",
        graph.names()
    );
    assert_eq!(graph.target_of(graph.placeholder("pending_symbol", "b")).0, container("krate::a"));
    assert_eq!(graph.target_of(graph.placeholder("pending_symbol", "f")).0, container("krate::a"));
}

// --- calls and references -------------------------------------------------------

#[test]
fn a_call_inside_one_file_is_a_direct_resolved_edge() {
    let krate = Crate::new(&[("src/lib.rs", "fn helper() {}\npub fn run() { helper(); }\n")]);
    let graph = krate.extract("src/lib.rs");
    let call = graph.edges(EdgeKind::Calls);
    assert_eq!(call.len(), 1, "{call:#?}");
    assert!(call[0].resolved, "within one file nothing is left for core to confirm");
    assert_eq!(graph.by_id(&call[0].to_id).qualified_name, "helper");
}

#[test]
fn a_module_qualified_path_call_is_a_name_placeholder_in_that_module() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod a;\npub mod b;\n"),
        ("src/a.rs", "pub fn f() {}\n"),
        ("src/b.rs", "pub fn run() { crate::a::f(); }\n"),
    ]);
    let graph = krate.extract("src/b.rs");
    let placeholder = graph.placeholder("pending_symbol", "f");
    assert_eq!(
        graph.target_of(placeholder),
        (container("krate::a"), TargetKey::Name("f".into())),
        "a name key, so core's re-export walk can follow a `pub use` chain"
    );
    assert!(graph.edges(EdgeKind::Calls).iter().all(|edge| !edge.resolved));
}

#[test]
fn a_type_qualified_path_call_is_a_qualified_name_placeholder() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod a;\npub mod b;\n"),
        ("src/a.rs", "pub struct Reader;\nimpl Reader { pub fn new() {} }\n"),
        ("src/b.rs", "use crate::a::Reader;\npub fn run() { Reader::new(); }\n"),
    ]);
    let graph = krate.extract("src/b.rs");
    assert_eq!(
        graph.targets(EdgeKind::Calls, "b::run"),
        vec!["pending_symbol krate::a::a::Reader::new"],
        "exact, because a module full of `new`s has nothing to tell them apart by name"
    );
    let placeholder = graph.placeholder("pending_symbol", "new");
    assert_eq!(
        graph.target_of(placeholder),
        (container("krate::a"), TargetKey::QualifiedName("a::Reader::new".into()))
    );
}

#[test]
fn self_and_self_dot_inside_an_impl_reach_the_impl_types_own_method() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub struct P;\nimpl P {\n  fn make() {}\n  fn helper(&self) {}\n  \
         pub fn run(&self) { Self::make(); self.helper(); }\n}\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(graph.targets(EdgeKind::Calls, "P::run"), vec!["P::helper", "P::make"]);
    assert!(graph.edges(EdgeKind::Calls).iter().all(|edge| edge.resolved));
}

/// Acceptance: a receiver call produces **no edge**. It is the shape nearly
/// every Rust method call has, and guessing at it is what the semantic tier
/// exists to avoid needing.
#[test]
fn a_receiver_call_produces_no_edge_and_one_open_site() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub struct P;\nimpl P { pub fn m(&self) {} }\npub fn run(p: P) { p.m(); }\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    let run = graph.node("run").id.clone();
    assert!(
        graph.edges(EdgeKind::Calls).iter().all(|edge| edge.from_id != run),
        "no edge may be emitted for `p.m()`: {:#?}",
        graph.edges(EdgeKind::Calls)
    );
    let sites: Vec<_> =
        graph.0.open_sites.iter().filter(|site| site.kind == OpenSiteKind::ReceiverCall).collect();
    assert_eq!(sites.len(), 1, "{sites:#?}");
    assert_eq!(sites[0].name, "m");
    assert_eq!(sites[0].from_id, run);
    assert_eq!(sites[0].edge_kind, EdgeKind::Calls);
    assert_eq!(sites[0].from_container.as_deref(), Some("krate"));
}

/// Decision 1, the trap this plugin is most at risk of: a local must never
/// become a module-scoped placeholder.
#[test]
fn locals_parameters_and_generics_never_become_placeholders() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub fn f() {}
pub fn run<T>(f: T, items: Vec<T>) {
    let helper = 1;
    let closure = |f: u8| f + helper;
    for helper in items { let _ = helper; }
    match f { other => { let _ = other; } }
    if let Some(bound) = None::<u8> { let _ = bound; }
    let _ = closure;
}
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    assert!(
        graph.0.nodes.iter().all(|node| node.native_kind.as_deref() != Some("pending_symbol")),
        "every name in `run` is local: {:#?}",
        graph.names()
    );
    let run = graph.node("run").id.clone();
    assert!(
        graph.0.edges.iter().all(|edge| edge.from_id != run || edge.kind == EdgeKind::Defines),
        "the parameter `f` shadows the function `f`: {:#?}",
        graph.0.edges
    );
}

#[test]
fn a_reference_to_a_type_declared_in_this_file_is_a_resolved_edge() {
    let krate = Crate::new(&[("src/lib.rs", "pub struct P;\npub fn run(p: P) -> P { p }\n")]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(graph.targets(EdgeKind::References, "run"), vec!["P"]);
}

#[test]
fn a_macro_invocation_of_a_macro_this_file_defines_is_a_call() {
    let krate =
        Crate::new(&[("src/lib.rs", "macro_rules! mac { () => {}; }\npub fn run() { mac!(); write!(); }\n")]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(
        graph.targets(EdgeKind::Calls, "run"),
        vec!["mac"],
        "`write!` is not this project's, and is not guessed at"
    );
}

// --- implementations ------------------------------------------------------------

#[test]
fn impl_trait_for_type_is_a_supertype_edge_from_the_type_to_the_trait() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod traits;\npub struct P;\nimpl crate::traits::Shape for P {}\n"),
        ("src/traits.rs", "pub trait Shape {}\n"),
    ]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(
        graph.targets(EdgeKind::SupertypeOf, "P"),
        vec!["pending_symbol krate::traits::Shape"],
        "subtype -> supertype, the direction find_implementations walks"
    );
}

#[test]
fn a_same_file_trait_impl_and_a_supertrait_bound_are_both_supertype_edges() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub trait Base {}\npub trait Extra: Base {}\npub struct P;\nimpl Base for P {}\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(graph.targets(EdgeKind::SupertypeOf, "P"), vec!["Base"]);
    assert_eq!(graph.targets(EdgeKind::SupertypeOf, "Extra"), vec!["Base"]);
    assert!(graph.edges(EdgeKind::SupertypeOf).iter().all(|edge| edge.resolved));
}

/// Decision 7: the edge would have to start at a node of another file, which
/// no structural diff may do - so the question is recorded for the semantic
/// tier, whose diffs may cross files.
#[test]
fn an_impl_for_a_type_from_another_file_is_an_open_site_rather_than_an_edge() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod types;\npub trait Shape {}\nimpl Shape for crate::types::P {}\n"),
        ("src/types.rs", "pub struct P;\n"),
    ]);
    let graph = krate.extract("src/lib.rs");
    assert!(graph.edges(EdgeKind::SupertypeOf).is_empty(), "{:#?}", graph.edges(EdgeKind::SupertypeOf));
    let sites: Vec<_> =
        graph.0.open_sites.iter().filter(|site| site.kind == OpenSiteKind::Implementation).collect();
    assert_eq!(sites.len(), 1, "{sites:#?}");
    assert_eq!(sites[0].name, "P");
    assert_eq!(sites[0].edge_kind, EdgeKind::SupertypeOf);
}

/// Decision 8 (GM-361): a blanket impl implements the trait for something
/// that is not a declaration of this project at all, so the `impl` block
/// itself carries the edge.
///
/// Both shapes measured missing from `find_implementations("Sink")` on
/// ripgrep are here: a self type that is not a path (`&'a mut S`), and one
/// whose head is a path naming nothing this file declares or imports
/// (`Box<S>`). The block's name is the prefix its own methods already carry,
/// which is what makes `<&'a mut S as Sink>` and
/// `<&'a mut S as Sink>::accept` read as one thing.
#[test]
fn a_blanket_impl_is_a_supertype_edge_from_the_impl_block_itself() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub trait Sink { fn accept(&self) -> u8; }\n\
         impl<'a, S: Sink> Sink for &'a mut S { fn accept(&self) -> u8 { (**self).accept() } }\n\
         impl<S: Sink + ?Sized> Sink for Box<S> { fn accept(&self) -> u8 { (**self).accept() } }\n",
    )]);
    let graph = krate.extract("src/lib.rs");

    for name in ["<&'a mut S as Sink>", "<Box as Sink>"] {
        let block = graph.node(name);
        assert_eq!(block.kind, NodeKind::Type, "{name} is what find_implementations reports");
        assert_eq!(block.native_kind.as_deref(), Some("impl"), "and an `impl`, not a struct");
        assert_eq!(graph.targets(EdgeKind::SupertypeOf, name), vec!["Sink"]);
    }
    assert!(
        graph.edges(EdgeKind::SupertypeOf).iter().all(|edge| edge.resolved),
        "both ends are declarations of this same file"
    );
    assert!(
        graph.0.open_sites.iter().all(|site| site.kind != OpenSiteKind::Implementation),
        "neither is a question for the semantic tier - see Decision 8: {:#?}",
        graph.0.open_sites
    );
}

/// The other half of Decision 8, and the reason it is not simply "declare a
/// block whenever the self type does not resolve here": `P` below **is** a
/// declaration this project makes, one file over, and the answer a reader
/// wants for `impl Shape for P` is `P` - which the semantic tier's trait
/// sweep produces from the open site's own file. A block node here would be
/// a second row describing the one impl.
#[test]
fn an_impl_for_an_imported_type_keeps_its_open_site_and_gets_no_block_node() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod types;\npub trait Shape {}\nuse crate::types::P;\nimpl Shape for P {}\n"),
        ("src/types.rs", "pub struct P;\n"),
    ]);
    let graph = krate.extract("src/lib.rs");
    assert!(graph.edges(EdgeKind::SupertypeOf).is_empty(), "{:#?}", graph.edges(EdgeKind::SupertypeOf));
    assert_eq!(graph.find("<P as Shape>"), None, "no block node: {:#?}", graph.names());
    let sites: Vec<_> =
        graph.0.open_sites.iter().filter(|site| site.kind == OpenSiteKind::Implementation).collect();
    assert_eq!(sites.len(), 1, "{sites:#?}");
    assert_eq!(sites[0].name, "P");
}

// --- cfg, errors, purity ---------------------------------------------------------

/// Decision 6: every alternative is indexed, and two alternatives that are
/// the same Rust path are one node rather than one line written twice.
#[test]
fn two_cfg_alternatives_of_one_item_are_one_node_and_do_not_collide() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "#[cfg(unix)]\npub fn open() {}\n#[cfg(windows)]\npub fn open() {}\n\
         #[cfg(unix)]\npub mod imp;\n#[cfg(windows)]\npub mod imp;\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(graph.0.nodes.iter().filter(|node| node.qualified_name == "open").count(), 1);
    assert_eq!(graph.0.nodes.iter().filter(|node| node.qualified_name == "imp").count(), 1);
    let ids: Vec<&str> = graph.0.nodes.iter().map(|node| node.id.as_str()).collect();
    let unique: std::collections::BTreeSet<&str> = ids.iter().copied().collect();
    assert_eq!(ids.len(), unique.len(), "no id is written twice");
}

#[test]
fn a_syntax_error_keeps_the_files_declarations_and_marks_them() {
    let krate = Crate::new(&[("src/lib.rs", "pub fn a() {}\npub fn b( {\npub fn c() {}\n")]);
    let graph = krate.extract("src/lib.rs");
    assert!(graph.find("a").is_some(), "{:#?}", graph.names());
    assert!(graph.0.nodes.iter().all(|node| node.has_syntax_errors));
}

#[test]
fn extraction_is_a_pure_function_of_the_source() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub mod a;\nuse crate::a::f;\npub struct P;\nimpl P { fn m(&self) { f(); self.m(); } }\n",
    )]);
    let once = krate.extract("src/lib.rs");
    let twice = krate.extract("src/lib.rs");
    assert_eq!(once.0, twice.0);
}

/// The `File` node's end is the one position a space before the last newline
/// cannot move - `id-stability.whitespace-edit`'s whole premise.
#[test]
fn a_trailing_space_before_the_last_newline_changes_nothing() {
    let krate = Crate::new(&[("src/lib.rs", "pub fn a() {}\n")]);
    let plain = RustExtractor.extract(&krate.project, &RelPath::new("src/lib.rs"), "pub fn a() {}\n");
    let spaced = RustExtractor.extract(&krate.project, &RelPath::new("src/lib.rs"), "pub fn a() {} \n");
    assert_eq!(plain, spaced);
}

/// An orphan file - one no crate's module tree reaches - is still indexed,
/// under the synthetic container `crate::project` gives it.
#[test]
fn an_orphan_file_is_indexed_under_its_synthetic_container() {
    let krate = Crate::new(&[("src/lib.rs", "pub fn f() {}\n"), ("src/loose.rs", "pub fn g() {}\n")]);
    let graph = krate.extract("src/loose.rs");
    let node = graph.node("g");
    assert_eq!(node.container.as_deref(), Some("orphan:src/loose.rs"));
    assert_eq!(node.container_parent, None);
}
