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

/// Every `reexport` row of `graph`, as `(container, published name, target,
/// visibility)`, sorted.
fn reexport_rows(graph: &Graph) -> Vec<(String, String, (TargetScope, TargetKey), Visibility)> {
    let mut rows: Vec<_> = graph
        .0
        .nodes
        .iter()
        .filter(|node| node.native_kind.as_deref() == Some("reexport"))
        .map(|node| {
            (
                node.container.clone().unwrap_or_default(),
                node.name.clone(),
                graph.target_of(node),
                node.visibility.clone(),
            )
        })
        .collect();
    rows.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    rows
}

/// A private `use` is a `reexport` row that only its own module and that
/// module's descendants may follow: `container(<module>)`. A glob always is
/// one; a named leaf only in a module that declares a child module, since
/// only a descendant can follow it (docs/architecture/gm-479-use-super-private-imports.md).
///
/// Controls: emit no row for a private `use` in `Declarer::use_leaf`, and
/// the `user` and `tests` rows are missing; drop the `has_child_modules`
/// condition, and `leaf` gains a named row; emit private rows as
/// `Visibility::File`, and the visibilities differ.
#[test]
fn a_private_use_is_a_reexport_row_visible_in_its_own_module_only() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod a;\npub mod user;\npub mod leaf;\n"),
        ("src/a.rs", "pub struct P;\n"),
        (
            "src/user.rs",
            "use crate::a::P;\nuse crate::a::P as Q;\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n}\n",
        ),
        ("src/leaf.rs", "use crate::a::P;\nuse crate::a::*;\n"),
    ]);
    let private = |module: &str| Visibility::Container(module.to_string());

    let user = krate.extract("src/user.rs");
    assert_eq!(
        reexport_rows(&user),
        vec![
            (
                "krate::user".into(),
                "P".into(),
                (container("krate::a"), TargetKey::Name("P".into())),
                private("krate::user")
            ),
            (
                "krate::user".into(),
                "Q".into(),
                (container("krate::a"), TargetKey::Name("P".into())),
                private("krate::user")
            ),
            (
                "krate::user::tests".into(),
                "*".into(),
                (container("krate::user"), TargetKey::Name("*".into())),
                private("krate::user::tests")
            ),
        ]
    );

    let leaf = krate.extract("src/leaf.rs");
    assert_eq!(
        reexport_rows(&leaf),
        vec![(
            "krate::leaf".into(),
            "*".into(),
            (container("krate::a"), TargetKey::Name("*".into())),
            private("krate::leaf")
        )],
        "a module with no child module gets its glob row and no named one"
    );
}

/// The named `reexport` rows of `graph` - its globs left out - as
/// `(container, published name, target container, target name)`.
fn named_reexport_rows(graph: &Graph) -> Vec<(String, String, TargetScope, String)> {
    reexport_rows(graph)
        .into_iter()
        .filter(|(_, published, ..)| published != "*")
        .map(|(module, published, (scope, key), visibility)| {
            assert_eq!(visibility, Visibility::Container(module.clone()), "{module} {published}");
            let TargetKey::Name(name) = key else { panic!("{module} {published}: {key:?}") };
            (module, published, scope, name)
        })
        .collect()
}

/// An external named `use` is a private row onto its crate path (`std::io`
/// for `use std::io::Error;`) only in a module that has a child module *and*
/// a glob - the one place an explicit import has a glob to shadow for a
/// descendant. The glob may sit below the `use`, and an inline module's own
/// `use` lines count for that module, not its parent.
///
/// Controls: emit no row for an external named `use` in
/// `Declarer::use_leaf` - `both` and `inline::inner` lose theirs; drop the
/// `has_glob` condition - `no_glob` gains one; drop the `has_child_modules`
/// condition - `no_child` gains one; target `krate` instead of
/// `external_path(&leaf.prefix)` - the targets read `std`; drop the
/// `use_declaration` arm of `Declarer::collect_modules` - every row is gone.
#[test]
fn an_external_named_use_is_a_row_only_where_a_glob_and_a_child_module_are() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod x;\npub mod both;\npub mod no_glob;\npub mod no_child;\npub mod inline;\n"),
        ("src/x.rs", "pub struct Error;\n"),
        (
            "src/both.rs",
            "use std::io::Error;\nuse std::fmt::Result as FmtResult;\nuse crate::x::*;\n\n\
             #[cfg(test)]\nmod tests {\n    use super::*;\n}\n",
        ),
        ("src/no_glob.rs", "use std::io::Error;\n\nmod tests {}\n"),
        ("src/no_child.rs", "use std::io::Error;\nuse crate::x::*;\n"),
        (
            "src/inline.rs",
            "use crate::x::*;\n\nmod inner {\n    use std::io::Error;\n    use crate::x::*;\n\n    mod deep {}\n}\n",
        ),
    ]);

    assert_eq!(
        named_reexport_rows(&krate.extract("src/both.rs")),
        vec![
            ("krate::both".into(), "Error".into(), container("std::io"), "Error".into()),
            ("krate::both".into(), "FmtResult".into(), container("std::fmt"), "Result".into()),
        ]
    );
    assert_eq!(named_reexport_rows(&krate.extract("src/no_glob.rs")), vec![], "no glob to shadow");
    assert_eq!(named_reexport_rows(&krate.extract("src/no_child.rs")), vec![], "no descendant to follow it");
    assert_eq!(
        named_reexport_rows(&krate.extract("src/inline.rs")),
        vec![("krate::inline::inner".into(), "Error".into(), container("std::io"), "Error".into())]
    );
}

/// Two inline modules of one file importing the same item are two rows, one
/// per module: the row's id names its module.
///
/// Control: drop the `{container}: ` prefix from `Emitter::reexport`'s
/// `qualifiedName` - the second module's row is lost.
#[test]
fn two_modules_of_one_file_importing_one_item_get_a_row_each() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod a;\npub mod two;\n"),
        ("src/a.rs", "pub struct P;\n"),
        (
            "src/two.rs",
            "pub mod x {\n    pub use crate::a::P;\n}\npub mod y {\n    pub use crate::a::P;\n}\n",
        ),
    ]);
    let two = krate.extract("src/two.rs");
    let modules: Vec<_> = reexport_rows(&two).into_iter().map(|(module, name, ..)| (module, name)).collect();
    assert_eq!(
        modules,
        vec![("krate::two::x".to_string(), "P".to_string()), ("krate::two::y".to_string(), "P".to_string())]
    );
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

/// A bare `y()` never names an associated item, so a method or trait item
/// `y` beside the free `fn y` does not make it ambiguous; the methods keep
/// their own type-qualified callers.
///
/// Control: in `Declarer::declare` (`extractor::decls`), record every
/// declaration with `model.declare` again, ignoring `block` - `run` and
/// `Tr::d` lose their edge to `y`.
#[test]
fn a_bare_call_lands_on_the_free_fn_beside_same_named_associated_items() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub fn y() {}
pub struct T;
impl T {
    pub fn y(&self) {}
    pub fn z(&self) { y(); Self::y(self); T::y(self); }
}
pub trait Tr {
    fn y();
    fn d() { y(); }
}
impl Tr for T { fn y() {} }
pub fn run() { y(); T::y(&T); }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(graph.targets(EdgeKind::Calls, "run"), vec!["T::y", "y"]);
    assert_eq!(graph.targets(EdgeKind::Calls, "T::z"), vec!["T::y", "y"]);
    assert_eq!(graph.targets(EdgeKind::Calls, "Tr::d"), vec!["y"]);
    let free = graph.node("y").id.clone();
    assert!(
        graph.edges(EdgeKind::Calls).iter().filter(|edge| edge.to_id == free).all(|edge| edge.resolved),
        "a same-file target is resolved"
    );
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

// --- struct fields (docs/architecture/gm-450-rust-fields.md) -------------------

/// A named field is a `Variable`/`field` node named `T.f` within its module,
/// beside the inherent methods; tuple-struct and enum-variant fields are not
/// nodes. Its uses are references: `self.f` in the impl and a literal's or
/// pattern's field names by address, `x.f` on any other receiver as an open
/// site for the semantic tier.
#[test]
fn struct_fields_are_nodes_and_their_uses_are_references() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod store;\npub mod user;\n"),
        (
            "src/store.rs",
            r#"
pub struct Ledger {
    /// Whether every row is unresolved.
    pub all_unresolved: bool,
    pub truncated_by: Option<u8>,
    secret: u8,
}
pub struct Pair(pub u8, u8);
pub enum Shape { Square { side: u8 } }
impl Ledger {
    pub fn settle(&self) -> bool { self.all_unresolved && self.truncated_by.is_none() }
}
pub fn truncated_by() {}
pub fn pick() -> fn() { truncated_by }
"#,
        ),
        (
            "src/user.rs",
            r#"
use crate::store::Ledger;
pub fn tally() -> bool {
    let ledger = crate::store::Ledger { all_unresolved: true, truncated_by: None };
    ledger.settle() && ledger.all_unresolved
}
pub fn drain(ledger: Ledger) -> Option<u8> {
    let Ledger { truncated_by, .. } = ledger;
    truncated_by
}
"#,
        ),
    ]);
    let store = krate.extract("src/store.rs");
    let field = store.node("store::Ledger.all_unresolved");
    assert_eq!((field.kind, field.native_kind.as_deref()), (NodeKind::Variable, Some("field")));
    assert_eq!(field.name, "all_unresolved");
    assert_eq!(field.signature.as_deref(), Some("pub all_unresolved: bool"));
    assert_eq!(field.doc_comment.as_deref(), Some("Whether every row is unresolved."));
    assert_eq!(field.visibility, Visibility::Public);
    assert_eq!(field.container.as_deref(), store.node("store::Ledger::settle").container.as_deref());
    assert_eq!(store.node("store::Ledger.truncated_by").native_kind.as_deref(), Some("field"));
    assert_eq!(store.node("store::Ledger.secret").visibility, Visibility::Container("krate::store".into()));
    assert_eq!(store.node("store::Ledger::settle").native_kind.as_deref(), Some("method"));
    let fields: Vec<_> =
        store.0.nodes.iter().filter(|node| node.native_kind.as_deref() == Some("field")).collect();
    assert_eq!(fields.len(), 3, "only Ledger's named fields: {:#?}", store.names());

    // `self.f` inside `impl Ledger` lands on the field, same file.
    assert_eq!(
        store.targets(EdgeKind::References, "store::Ledger::settle"),
        vec!["store::Ledger.all_unresolved", "store::Ledger.truncated_by"]
    );
    // A field is never a bare name: `truncated_by` here is the free function.
    assert_eq!(store.targets(EdgeKind::References, "store::pick"), vec!["store::truncated_by"]);

    let user = krate.extract("src/user.rs");
    // The literal's type, and its two fields by qualifiedName in `store`.
    let tally = user.targets(EdgeKind::References, "user::tally");
    assert_eq!(
        tally,
        vec![
            "pending_symbol krate::store::Ledger",
            "pending_symbol krate::store::store::Ledger.all_unresolved",
            "pending_symbol krate::store::store::Ledger.truncated_by",
        ]
    );
    for name in ["all_unresolved", "truncated_by"] {
        let placeholder = user.placeholder("pending_symbol", name);
        assert_eq!(
            user.target_of(placeholder),
            (container("krate::store"), TargetKey::QualifiedName(format!("store::Ledger.{name}")))
        );
    }
    // The trailing `ledger.all_unresolved` read is a question for the
    // semantic tier, at the field name.
    let reads: Vec<_> = user
        .0
        .open_sites
        .iter()
        .filter(|site| site.kind == OpenSiteKind::Reference && site.name == "all_unresolved")
        .collect();
    assert_eq!(reads.len(), 1, "{:#?}", user.0.open_sites);
    assert_eq!(reads[0].edge_kind, EdgeKind::References);
    // A destructuring pattern names the field too.
    assert!(
        user.targets(EdgeKind::References, "user::drain")
            .contains(&"pending_symbol krate::store::store::Ledger.truncated_by".to_string()),
        "{:?}",
        user.targets(EdgeKind::References, "user::drain")
    );
}

/// A getter named like its field: the field is `T.f`, the method `T::f`, two
/// nodes under two keys. Calls - `self.f()`, `Self::f(..)`, `T::f(..)` from
/// another file - reach the method; `self.f`, a literal's and a pattern's
/// field names reach the field; the method's own body is attributed to the
/// method, so the field has no outgoing edge at all.
#[test]
fn a_getter_named_like_its_field_keeps_its_calls_and_the_field_keeps_its_references() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod store;\npub mod user;\n"),
        (
            "src/store.rs",
            r#"
pub struct Holder {
    pub inner: u8,
}
impl Holder {
    pub fn inner(&self) -> u8 { self.inner }
    pub fn twice(&self) -> u8 { self.inner() + Self::inner(self) }
}
"#,
        ),
        (
            "src/user.rs",
            r#"
use crate::store::Holder;
pub fn make() -> Holder { Holder { inner: 1 } }
pub fn get(holder: &Holder) -> u8 { Holder::inner(holder) }
pub fn take(holder: Holder) -> u8 { let Holder { inner } = holder; inner }
"#,
        ),
    ]);
    let store = krate.extract("src/store.rs");
    let field = store.node("store::Holder.inner");
    let method = store.node("store::Holder::inner");
    assert_eq!((field.kind, field.native_kind.as_deref()), (NodeKind::Variable, Some("field")));
    assert_eq!((method.kind, method.native_kind.as_deref()), (NodeKind::Function, Some("method")));
    assert_ne!(field.id, method.id);

    // The getter's body is the getter's: one reference, to the field.
    assert_eq!(store.targets(EdgeKind::References, "store::Holder::inner"), vec!["store::Holder.inner"]);
    let field_id = field.id.clone();
    let out_of_field: Vec<_> = store.0.edges.iter().filter(|edge| edge.from_id == field_id).collect();
    assert!(out_of_field.is_empty(), "a field has no body: {out_of_field:#?}");
    // Both calls in `twice` reach the method, never the field.
    let mut calls = store.targets(EdgeKind::Calls, "store::Holder::twice");
    calls.dedup();
    assert_eq!(calls, vec!["store::Holder::inner"]);
    assert!(store.targets(EdgeKind::References, "store::Holder::twice").is_empty());

    let user = krate.extract("src/user.rs");
    let field_ref = "pending_symbol krate::store::store::Holder.inner".to_string();
    let method_ref = "pending_symbol krate::store::store::Holder::inner".to_string();
    assert!(user.targets(EdgeKind::References, "user::make").contains(&field_ref));
    assert!(user.targets(EdgeKind::References, "user::take").contains(&field_ref));
    assert_eq!(user.targets(EdgeKind::Calls, "user::get"), vec![method_ref]);
    assert!(!user.targets(EdgeKind::References, "user::get").contains(&field_ref));
    let keys: Vec<_> = user
        .0
        .nodes
        .iter()
        .filter(|node| node.native_kind.as_deref() == Some("pending_symbol") && node.name == "inner")
        .map(|node| user.target_of(node))
        .collect();
    assert!(
        keys.contains(&(container("krate::store"), TargetKey::QualifiedName("store::Holder.inner".into())))
    );
    assert!(
        keys.contains(&(container("krate::store"), TargetKey::QualifiedName("store::Holder::inner".into())))
    );
}

// --- GM-472: members used through a `pub use` ---------------------------------

/// The GM-472 fixture, as source: `a` declares `T` with a field `f` and a
/// method `m`; `named` republishes it by a named `pub use` (and once more
/// under an alias), `glob` by `pub use crate::a::*`, and `outer` globs
/// `named`, so reaching `T` from `outer` is a two-hop chain. Each `user_*`
/// file uses `T.f` (a struct literal) and `T::m` (a path call) through one of
/// those paths. `core/src/graph/symbol_links/tests.rs`' GM-472 tests replay
/// exactly the rows asserted here through the linker.
fn gm472_crate() -> Crate {
    Crate::new(&[
        ("src/lib.rs", "pub mod a;\npub mod named;\npub mod glob;\npub mod outer;\npub mod user_named;\npub mod user_glob;\npub mod user_renamed;\npub mod user_outer;\n"),
        ("src/a.rs", "pub struct T {\n    pub f: u32,\n}\n\nimpl T {\n    pub fn m(&self) -> u32 {\n        self.f\n    }\n}\n"),
        ("src/named.rs", "pub use crate::a::T;\npub use crate::a::T as Renamed;\n"),
        ("src/glob.rs", "pub use crate::a::*;\n"),
        ("src/outer.rs", "pub use crate::named::*;\n"),
        ("src/user_named.rs", "use crate::named::T;\n\npub fn run() -> u32 {\n    let t = T { f: 1 };\n    T::m(&t)\n}\n"),
        ("src/user_glob.rs", "use crate::glob::T;\n\npub fn run() -> u32 {\n    let t = T { f: 1 };\n    T::m(&t)\n}\n"),
        ("src/user_renamed.rs", "use crate::named::Renamed;\n\npub fn run() -> u32 {\n    let t = Renamed { f: 1 };\n    Renamed::m(&t)\n}\n"),
        ("src/user_outer.rs", "use crate::outer::T;\n\npub fn run() -> u32 {\n    let t = T { f: 1 };\n    T::m(&t)\n}\n"),
    ])
}

/// The plugin half of a member used through a re-export. The address names
/// the *re-exporting* module (where the `use` says `T` lives) by
/// `qualifiedName`, with its `keyPath`, and that module declares no `T` -
/// only a `reexport` node publishing it. The linker follows the re-export
/// from the path's head (docs/architecture/gm-472-reexport-links.md).
#[test]
fn gm472_a_member_used_through_a_pub_use_is_addressed_at_the_reexporting_module() {
    let krate = gm472_crate();

    let a = krate.extract("src/a.rs");
    assert_eq!(a.node("a::T.f").native_kind.as_deref(), Some("field"));
    assert_eq!(a.node("a::T::m").native_kind.as_deref(), Some("method"));

    let named = krate.extract("src/named.rs");
    let reexports: Vec<_> = named
        .0
        .nodes
        .iter()
        .filter(|node| node.native_kind.as_deref() == Some("reexport"))
        .map(|node| (node.name.clone(), named.target_of(node)))
        .collect();
    assert_eq!(
        reexports,
        vec![
            ("T".to_string(), (container("krate::a"), TargetKey::Name("T".into()))),
            ("Renamed".to_string(), (container("krate::a"), TargetKey::Name("T".into()))),
        ]
    );
    let glob = krate.extract("src/glob.rs");
    assert_eq!(
        glob.target_of(glob.placeholder("reexport", "*")),
        (container("krate::a"), TargetKey::Name("*".into()))
    );
    let outer = krate.extract("src/outer.rs");
    assert_eq!(
        outer.target_of(outer.placeholder("reexport", "*")),
        (container("krate::named"), TargetKey::Name("*".into()))
    );

    for (file, module, head) in [
        ("src/user_named.rs", "named", "T"),
        ("src/user_glob.rs", "glob", "T"),
        ("src/user_renamed.rs", "named", "Renamed"),
        ("src/user_outer.rs", "outer", "T"),
    ] {
        let user = krate.extract(file);
        let scope = container(&format!("krate::{module}"));
        assert_eq!(
            user.target_of(user.placeholder("pending_symbol", "f")),
            (scope.clone(), TargetKey::QualifiedName(format!("{module}::{head}.f"))),
            "{file}: the field, by qualifiedName in the module the `use` named"
        );
        assert_eq!(
            user.target_of(user.placeholder("pending_symbol", "m")),
            (scope, TargetKey::QualifiedName(format!("{module}::{head}::m"))),
            "{file}: the method, the same way"
        );
        // The segments core splits into head and member: never the string.
        for (member, sep) in [("f", "."), ("m", "::")] {
            let path = user.placeholder("pending_symbol", member).target.as_ref().unwrap().key_path.as_ref();
            assert_eq!(
                path.map(segments),
                Some(vec![
                    (String::new(), module.to_string()),
                    ("::".to_string(), head.to_string()),
                    (sep.to_string(), member.to_string()),
                ]),
                "{file}: the keyPath of {member}"
            );
        }
    }
}

/// The segments of a path as `(sep, name)` pairs, `""` for the first.
fn segments(path: &g_mesh_plugin_sdk::wire::QualifiedPath) -> Vec<(String, String)> {
    path.segments()
        .iter()
        .map(|segment| (segment.sep.clone().unwrap_or_default(), segment.name.clone()))
        .collect()
}

/// Every declaration carries a `qualifiedPath` that joins back to its
/// `qualifiedName` and ends in its name; placeholders, the `File` node and
/// `external_module` nodes carry none; every `qualifiedName`-keyed
/// placeholder carries a `keyPath` that joins back to its key.
fn assert_paths_are_well_formed(graph: &Graph) {
    const PATHLESS: [&str; 5] = ["pending_symbol", "reexport", "resolved_module", "external_module", "file"];
    for node in &graph.0.nodes {
        let pathless = node.kind == NodeKind::File
            || node.native_kind.as_deref().is_some_and(|native| PATHLESS.contains(&native));
        if pathless {
            assert_eq!(node.qualified_path, None, "{node:#?}");
            assert!(node.alias_paths.is_empty(), "{node:#?}");
        } else {
            assert!(node.qualified_path.is_some(), "a declaration without a path: {node:#?}");
            assert_eq!(node.check_qualified_path(), Ok(()), "{node:#?}");
            for alias in &node.alias_paths {
                assert_eq!(node.check_alias_path(alias), Ok(()), "{node:#?}");
            }
        }
        if let Some(target) = &node.target {
            match &target.key {
                TargetKey::QualifiedName(_) => assert!(target.key_path.is_some(), "{node:#?}"),
                TargetKey::Name(_) => assert_eq!(target.key_path, None, "{node:#?}"),
            }
            assert_eq!(target.check_key_path(), Ok(()), "{node:#?}");
        }
    }
}

/// Module segments joined by `::`, a trait impl's `<X as T>` kept as one
/// segment, a field after `.`, a raw identifier as written; a trait-impl
/// member's only alias is the path with that segment replaced by the self
/// type's plain name, and no other member has one.
#[test]
fn every_declaration_carries_its_qualified_name_as_segments() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod store;\npub mod user;\n"),
        (
            "src/store.rs",
            r#"
pub trait Show { fn show(&self) -> u8; }
pub struct Holder<T> { pub inner: T }
impl<T> Holder<T> {
    pub fn inner(&self) -> u8 { 0 }
}
impl<'a, T> Show for &'a Holder<T> {
    fn show(&self) -> u8 { 1 }
}
impl Show for (u8, u8) {
    fn show(&self) -> u8 { 2 }
}
pub mod nested {
    pub fn r#type() {}
}
"#,
        ),
        (
            "src/user.rs",
            r#"
use crate::store::Holder;
pub fn make() -> Holder<u8> { Holder { inner: 1 } }
pub fn get(holder: &Holder<u8>) -> u8 { Holder::inner(holder) }
"#,
        ),
    ]);
    let store = krate.extract("src/store.rs");
    assert_paths_are_well_formed(&store);
    let user = krate.extract("src/user.rs");
    assert_paths_are_well_formed(&user);

    let pairs = |items: &[(&str, &str)]| -> Vec<(String, String)> {
        items.iter().map(|(sep, name)| (sep.to_string(), name.to_string())).collect()
    };
    let path_of =
        |qualified_name: &str| segments(store.node(qualified_name).qualified_path.as_ref().unwrap());

    assert_eq!(path_of("store::Holder.inner"), pairs(&[("", "store"), ("::", "Holder"), (".", "inner")]));
    assert_eq!(path_of("store::Holder::inner"), pairs(&[("", "store"), ("::", "Holder"), ("::", "inner")]));
    assert_eq!(path_of("store::nested::r#type"), pairs(&[("", "store"), ("::", "nested"), ("::", "r#type")]));
    let show = store.node("store::<&'a Holder<T> as Show>::show");
    assert_eq!(
        segments(show.qualified_path.as_ref().unwrap()),
        pairs(&[("", "store"), ("::", "<&'a Holder<T> as Show>"), ("::", "show")])
    );
    let aliases: Vec<_> = show.alias_paths.iter().map(segments).collect();
    assert_eq!(aliases, vec![pairs(&[("", "store"), ("::", "Holder"), ("::", "show")])]);

    // A self type with no single name has no alias; an inherent method or
    // a field never has one.
    assert!(store.node("store::<(u8, u8) as Show>::show").alias_paths.is_empty());
    assert!(store.node("store::Holder::inner").alias_paths.is_empty());
    assert!(store.node("store::Holder.inner").alias_paths.is_empty());

    // A cross-file field key and a method key keep their own last separator.
    let key_paths: Vec<_> = user
        .0
        .nodes
        .iter()
        .filter_map(|node| node.target.as_ref()?.key_path.as_ref())
        .map(segments)
        .collect();
    assert!(key_paths.contains(&pairs(&[("", "store"), ("::", "Holder"), (".", "inner")])), "{key_paths:?}");
    assert!(key_paths.contains(&pairs(&[("", "store"), ("::", "Holder"), ("::", "inner")])), "{key_paths:?}");
}
