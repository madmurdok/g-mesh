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
use g_mesh_plugin_sdk::{Extractor, FileGraph, OpenSite, OpenSiteKind, RelPath};

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

/// A receiver call whose receiver's type this file writes out is addressed
/// as `T::m`, and its open site stays, naming that edge in `replaces` so a
/// semantic answer that lands elsewhere can retract it.
#[test]
fn a_typed_receiver_call_produces_an_edge_and_an_open_site_that_replaces_it() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub struct P;\nimpl P { pub fn m(&self) {} }\npub fn run(p: P) { p.m(); }\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    let run = graph.node("run").id.clone();
    assert_eq!(graph.targets(EdgeKind::Calls, "run"), vec!["P::m"]);
    let edge = graph.edges(EdgeKind::Calls).into_iter().find(|edge| edge.from_id == run).unwrap().clone();
    assert!(edge.resolved, "a same-file target is a resolved edge");
    let sites: Vec<_> =
        graph.0.open_sites.iter().filter(|site| site.kind == OpenSiteKind::ReceiverCall).collect();
    assert_eq!(sites.len(), 1, "{sites:#?}");
    assert_eq!(sites[0].name, "m");
    assert_eq!(sites[0].from_id, run);
    assert_eq!(sites[0].edge_kind, EdgeKind::Calls);
    assert_eq!(sites[0].from_container.as_deref(), Some("krate"));
    assert_eq!(sites[0].replaces.as_deref(), Some(edge.id.as_str()), "the site names the edge it replaces");
}

/// A receiver whose type this file does not say - a generic parameter, a
/// trait object, `impl Trait`, a type from another crate, an untyped closure
/// parameter - still produces no edge, and its open site replaces nothing.
#[test]
fn an_untyped_receiver_call_produces_no_edge_and_an_open_site_that_replaces_nothing() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub trait Tr { fn m(&self); }
pub struct P;
impl P { pub fn m(&self) {} }
pub fn generic<T: Tr>(p: T) { p.m(); }
pub fn object(p: &dyn Tr) { p.m(); }
pub fn opaque(p: impl Tr) { p.m(); }
pub fn foreign(p: String) { p.m(); }
pub fn closure(ps: Vec<P>) { ps.iter().for_each(|p| p.m()); }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    for function in ["generic", "object", "opaque", "foreign", "closure"] {
        assert_eq!(graph.targets(EdgeKind::Calls, function), Vec::<String>::new(), "{function}");
    }
    let sites: Vec<_> = graph.0.open_sites.iter().filter(|site| site.name == "m").collect();
    assert_eq!(sites.len(), 5, "{sites:#?}");
    assert!(sites.iter().all(|site| site.replaces.is_none()), "{sites:#?}");
}

/// The Rust extractor opts into reporting untyped receiver calls, so
/// a call through an untyped closure parameter or a call chain reaches core
/// as the enclosing fn's `untypedCalls` (closures are not nodes), sorted and
/// deduplicated, while a typed receiver call, which already has its edge,
/// is not reported. Control: drop `graph.record_untyped_receiver_calls()`
/// from `Emitter::new` (`closure` and `chain` carry nothing).
#[test]
fn untyped_receiver_calls_reach_the_enclosing_fn_and_typed_ones_do_not() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub struct P;
impl P { pub fn m(&self) {} pub fn n(&self) {} }
pub fn closure(ps: Vec<P>) { ps.iter().for_each(|p| { p.n(); p.m(); p.m(); }); }
pub fn chain(ps: Vec<P>) { ps.first().unwrap().m(); }
pub fn typed(p: P) { p.m(); }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");

    // `iter`/`for_each` are untyped too: `Vec`'s methods, from another crate.
    assert_eq!(graph.node("closure").untyped_calls, ["for_each", "iter", "m", "n"]);
    assert!(graph.node("chain").untyped_calls.contains(&"m".to_string()), "{:?}", graph.node("chain"));
    assert_eq!(graph.targets(EdgeKind::Calls, "typed"), vec!["P::m"]);
    assert!(graph.node("typed").untyped_calls.is_empty(), "{:?}", graph.node("typed").untyped_calls);
    let json = serde_json::to_string(graph.node("typed")).unwrap();
    assert!(!json.contains("untypedCalls"), "{json}");
}

/// GM-485's own shape: a local typed by the return type of a method, which
/// is typed by the parameter it is called on.
#[test]
fn a_local_typed_by_a_same_file_method_return_links_its_receiver_calls() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub struct Q;
impl Q { pub fn refuses(&self) -> bool { true } }
pub struct A;
impl A { pub fn shapes(&self) -> &Q { &Q } }
pub fn run(a: &A) { let s = a.shapes(); s.refuses(); }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(graph.targets(EdgeKind::Calls, "run"), vec!["A::shapes", "Q::refuses"]);
    let sites: Vec<_> =
        graph.0.open_sites.iter().filter(|site| site.kind == OpenSiteKind::ReceiverCall).collect();
    assert_eq!(sites.len(), 2, "{sites:#?}");
    assert!(sites.iter().all(|site| site.replaces.is_some()), "{sites:#?}");
}

/// The same caller in another file: the types are imported, so `a.shapes()`
/// is a `qualifiedName` placeholder in `A`'s module, and `s` stays untyped
/// because `A::shapes`'s return type is written in another file.
#[test]
fn a_return_type_written_in_another_file_types_nothing() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod m;\npub mod caller;\n"),
        (
            "src/m.rs",
            "pub struct Q;\nimpl Q { pub fn refuses(&self) -> bool { true } }\n\
             pub struct A;\nimpl A { pub fn shapes(&self) -> &Q { &Q } }\n",
        ),
        ("src/caller.rs", "use crate::m::{A, Q};\npub fn run(a: &A) { let s = a.shapes(); s.refuses(); }\n"),
    ]);
    let graph = krate.extract("src/caller.rs");
    assert_eq!(graph.targets(EdgeKind::Calls, "caller::run"), vec!["pending_symbol krate::m::m::A::shapes"]);
    let placeholder = graph.placeholder("pending_symbol", "shapes");
    assert_eq!(
        graph.target_of(placeholder),
        (container("krate::m"), TargetKey::QualifiedName("m::A::shapes".into()))
    );
    let refuses = graph.0.open_sites.iter().find(|site| site.name == "refuses").unwrap();
    assert_eq!(refuses.replaces, None);
}

/// Every way a local is typed from what this file writes: a `let` type, a
/// struct literal, an alias, a free function's and an associated function's
/// return type, through `?`, `unwrap()` and `expect()`, and `Box`.
#[test]
fn locals_are_typed_by_annotations_literals_aliases_and_same_file_returns() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub struct P;
impl P {
    pub fn new() -> Self { P }
    pub fn maybe() -> Option<Self> { None }
    pub fn m(&self) {}
}
pub fn make() -> P { P }
pub fn fallible() -> Result<Box<P>, ()> { Err(()) }
pub fn annotated() { let p: P = Default::default(); p.m(); }
pub fn literal() { let p = P {}; p.m(); }
pub fn alias(q: &P) { let p = &q; p.m(); }
pub fn free() { let p = make(); p.m(); }
pub fn assoc() { let p = P::new(); p.m(); }
pub fn tried() -> Option<()> { let p = P::maybe()?; p.m(); None }
pub fn unwrapped() { let p = P::maybe().unwrap(); p.m(); }
pub fn expected() { let p = fallible().expect("p"); p.m(); }
pub fn boxed(p: Box<P>) { p.m(); }
pub fn later() { let p = P::maybe(); let q = p.unwrap(); q.m(); }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    for function in
        ["annotated", "literal", "alias", "free", "assoc", "tried", "unwrapped", "expected", "boxed", "later"]
    {
        let targets = graph.targets(EdgeKind::Calls, function);
        assert!(targets.contains(&"P::m".to_string()), "{function}: {targets:?}");
    }
}

/// What does not type a local: an `Option` never unwrapped, `Rc`/`Arc`, a
/// project type that happens to be called `Option`, a generic return, a
/// rebinding without a type, and a fourth hop.
#[test]
fn wrappers_shadowing_generics_and_long_chains_leave_a_local_untyped() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod a;\npub mod b;\n"),
        (
            "src/a.rs",
            r#"
use std::rc::Rc;
pub struct P;
impl P {
    pub fn maybe() -> Option<Self> { None }
    pub fn next(&self) -> P { P }
    pub fn m(&self) {}
}
pub fn any<P>() -> P { todo!() }
pub fn wrapped() { let p = P::maybe(); p.m(); }
pub fn counted(p: Rc<P>) { p.m(); }
pub fn generic() { let p: P = any(); let q = any(); q.m(); let _ = p; }
pub fn shadowed(p: P) { let p = 3; p.m(); }
pub fn closure(p: P) { let f = |p| p.m(); let _ = f; }
pub fn chained(p: P) { let a = p.next(); let b = a.next(); let c = b.next(); a.m(); b.m(); c.m(); }
"#,
        ),
        (
            "src/b.rs",
            r#"
pub struct Option<T>(pub T);
pub struct P;
impl P { pub fn m(&self) {} }
pub fn own() -> Option<P> { Option(P) }
pub fn run() { let p = own().unwrap(); p.m(); }
"#,
        ),
    ]);
    let a = krate.extract("src/a.rs");
    for function in ["a::wrapped", "a::counted", "a::generic", "a::shadowed", "a::closure"] {
        let targets = a.targets(EdgeKind::Calls, function);
        assert!(!targets.contains(&"a::P::m".to_string()), "{function}: {targets:?}");
    }
    let chained = a.targets(EdgeKind::Calls, "a::chained");
    assert_eq!(chained, vec!["a::P::m", "a::P::next"], "two hops are typed, the third is not");
    let chained_id = a.node("a::chained").id.clone();
    let untyped: Vec<_> =
        a.0.open_sites
            .iter()
            .filter(|site| site.from_id == chained_id && site.name == "m" && site.replaces.is_none())
            .collect();
    assert_eq!(untyped.len(), 1, "only `c.m()` is left untyped: {untyped:#?}");
    let b = krate.extract("src/b.rs");
    // `own()` is a typed receiver (GM-488): its `.unwrap()` is the project
    // `Option`'s own method, a placeholder since this file declares none.
    assert_eq!(
        b.targets(EdgeKind::Calls, "b::run"),
        vec!["b::own", "pending_symbol krate::b::b::Option::unwrap"],
        "a project `Option` is not unwrapped"
    );
}

/// Rust's method lookup prefers an inherent method to a trait method of the
/// same name, and `T::m` is the inherent one. A trait-impl method has no
/// `T::m` declaration, so it is a placeholder at that address: a miss, never
/// a link to the wrong method.
#[test]
fn a_typed_receiver_finds_the_inherent_method_and_misses_a_trait_impl_method() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub trait Tr { fn m(&self); fn t(&self); }
pub struct P;
impl P { pub fn m(&self) {} }
impl Tr for P { fn m(&self) {} fn t(&self) {} }
pub fn inherent(p: &P) { p.m(); }
pub fn traitish(p: &P) { p.t(); }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(graph.targets(EdgeKind::Calls, "inherent"), vec!["P::m"]);
    assert_eq!(graph.targets(EdgeKind::Calls, "traitish"), vec!["pending_symbol krate::P::t"]);
}

/// The receiver-call open sites named `name` out of `from`, in source order.
fn receiver_sites<'g>(graph: &'g Graph, from: &str, name: &str) -> Vec<&'g OpenSite> {
    let from = graph.node(from).id.clone();
    graph
        .0
        .open_sites
        .iter()
        .filter(|site| site.kind == OpenSiteKind::ReceiverCall && site.from_id == from && site.name == name)
        .collect()
}

/// GM-488 F1: a named field of a typed parameter or of `self` types its
/// receiver by the field's written type, through `Box` and `&`; each site
/// names the edge it replaces. Controls: drop the `field_expression` arm of
/// `Bodies::receiver_type` (nothing links); separately, skip
/// `set_field_type` in `Declarer::fields`'s named-field loop (nothing links).
#[test]
fn a_named_field_of_a_typed_value_or_of_self_types_its_receiver() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub struct Inner;
impl Inner { pub fn m(&self) {} }
pub struct Outer<'a> { inner: Inner, boxed: Box<Inner>, r: &'a Inner }
pub fn run(o: &Outer) { o.inner.m(); o.boxed.m(); o.r.m(); }
impl<'a> Outer<'a> { pub fn go(&self) { self.inner.m(); } }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    // One edge per (caller, target); the open sites count the calls.
    assert_eq!(graph.targets(EdgeKind::Calls, "run"), vec!["Inner::m"]);
    assert_eq!(graph.targets(EdgeKind::Calls, "Outer::go"), vec!["Inner::m"]);
    assert!(graph.edges(EdgeKind::Calls).iter().all(|edge| edge.resolved));
    for (from, calls) in [("run", 3), ("Outer::go", 1)] {
        let sites = receiver_sites(&graph, from, "m");
        assert_eq!(sites.len(), calls, "{from}: {sites:#?}");
        assert!(sites.iter().all(|site| site.replaces.is_some()), "{from}: {sites:#?}");
    }
}

/// GM-488 F2: a tuple struct's positional field, of `self` or of a typed
/// parameter. Control: drop the `ordered_field_declaration_list` branch of
/// `Declarer::fields` (neither links).
#[test]
fn a_positional_field_types_its_receiver() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub struct Inner;
impl Inner { pub fn m(&self) {} }
pub struct W(pub Inner);
impl W { pub fn go(&self) { self.0.m(); } }
pub fn f(w: W) { w.0.m(); }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(graph.targets(EdgeKind::Calls, "W::go"), vec!["Inner::m"]);
    assert_eq!(graph.targets(EdgeKind::Calls, "f"), vec!["Inner::m"]);
}

const CHAINS: &str = r#"
pub struct A;
pub struct B;
impl A {
    pub fn new() -> A { A }
    pub fn b(&self) -> B { B }
    pub fn maybe(&self) -> Option<B> { None }
}
impl B { pub fn m(&self) {} }
pub fn make() -> B { B }
pub fn run(a: A) -> Option<()> {
    a.b().m();
    A::new().b().m();
    make().m();
    a.maybe()?.m();
    a.maybe().unwrap().m();
    None
}
"#;

/// GM-488 F3: a call with a same-file written return type types its receiver
/// without a `let`: a method, an associated fn, a free fn, through `?` and
/// `unwrap()`. Control: drop the `call_expression`/`try_expression` arm of
/// `Bodies::receiver_type` (no `B::m`).
#[test]
fn a_call_with_a_written_return_type_types_its_receiver() {
    let krate = Crate::new(&[("src/lib.rs", CHAINS)]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(graph.targets(EdgeKind::Calls, "run"), vec!["A::b", "A::maybe", "A::new", "B::m", "make"]);
    // One edge per target; each of the five calls is a site that replaces it.
    let sites = receiver_sites(&graph, "run", "m");
    assert_eq!(sites.len(), 5, "{sites:#?}");
    assert!(sites.iter().all(|site| site.replaces.is_some()), "{sites:#?}");
}

/// GM-488 F4, GM-486's marker: an L4-typed receiver call has its edge, so it
/// is not reported in the enclosing fn's `untypedCalls`, whether typed by a
/// call or by a field. Control: the F3 revert (drop the
/// `call_expression`/`try_expression` arm of `Bodies::receiver_type`) puts
/// `m` back in `run`'s list; dropping the `field_expression` arm puts it back
/// in `field`'s.
#[test]
fn receiver_calls_typed_by_a_field_or_a_call_leave_untyped_calls() {
    let source = format!("{CHAINS}pub struct Holder {{ b: B }}\npub fn field(h: Holder) {{ h.b.m(); }}\n");
    let krate = Crate::new(&[("src/lib.rs", source.as_str())]);
    let graph = krate.extract("src/lib.rs");
    for function in ["run", "field"] {
        let untyped = &graph.node(function).untyped_calls;
        assert!(!untyped.contains(&"m".to_string()), "{function}: {untyped:?}");
    }
    assert!(graph.node("field").untyped_calls.is_empty(), "{:?}", graph.node("field").untyped_calls);
}

/// GM-488 F5 (D1): field hops and method-return hops share one budget of
/// `MAX_HOPS` = 2. Two hops link; a third leaves the call untyped, one open
/// site each that replaces nothing. Control: set `MAX_HOPS = 3` (the 3-hop
/// calls link to `D::m`).
#[test]
fn fields_and_calls_share_the_two_hop_budget() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub struct D;
impl D { pub fn m(&self) {} }
pub struct C { pub h: D }
impl C { pub fn m(&self) {} pub fn d(&self) -> D { D } }
pub struct B { pub g: C }
impl B { pub fn c(&self) -> C { C { h: D } } }
pub struct A;
impl A { pub fn b(&self) -> B { todo!() } }
pub struct F { pub f: B }
impl F {
    pub fn two_fields(&self) { self.f.g.m(); }
    pub fn three_fields(&self) { self.f.g.h.m(); }
}
pub fn two_calls(a: A) { a.b().c().m(); }
pub fn three_calls(a: A) { a.b().c().d().m(); }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(graph.targets(EdgeKind::Calls, "F::two_fields"), vec!["C::m"]);
    assert_eq!(graph.targets(EdgeKind::Calls, "two_calls"), vec!["A::b", "B::c", "C::m"]);
    assert_eq!(graph.targets(EdgeKind::Calls, "F::three_fields"), Vec::<String>::new());
    assert_eq!(graph.targets(EdgeKind::Calls, "three_calls"), vec!["A::b", "B::c", "C::d"]);
    for from in ["F::three_fields", "three_calls"] {
        let sites = receiver_sites(&graph, from, "m");
        assert_eq!(sites.len(), 1, "{from}: {sites:#?}");
        assert_eq!(sites[0].replaces, None, "{from}");
    }
}

/// GM-488 F6: a `let` bound to a field (by value or by reference) is typed
/// like the field. Control: drop the `field_expression` arm of
/// `Bodies::expression_type` (`y.m()` in `run` stays untyped; `&self.inner`
/// still links through the `reference_expression` arm).
#[test]
fn a_local_bound_to_a_field_is_typed_by_the_field() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub struct Inner;
impl Inner { pub fn m(&self) {} }
pub struct Outer { inner: Inner }
pub fn run(x: Outer) { let y = x.inner; y.m(); }
impl Outer { pub fn go(&self) { let z = &self.inner; z.m(); } }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(graph.targets(EdgeKind::Calls, "run"), vec!["Inner::m"]);
    assert_eq!(graph.targets(EdgeKind::Calls, "Outer::go"), vec!["Inner::m"]);
}

/// GM-488 F7: same-named fields of two structs are told apart by their
/// owner. Control: key `FileModel::field_types` by the bare field name (the
/// two `inner` entries collide and neither or the wrong one links).
#[test]
fn same_named_fields_of_different_structs_keep_their_own_types() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub struct P;
impl P { pub fn m(&self) {} }
pub struct Q;
impl Q { pub fn m(&self) {} }
pub struct X { inner: P }
pub struct Y { inner: Q }
pub fn run(x: X, y: Y) { x.inner.m(); y.inner.m(); }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(graph.targets(EdgeKind::Calls, "run"), vec!["P::m", "Q::m"]);
}

/// GM-488 N1: an `Option` field is not unwrapped implicitly, and `Rc`/`Vec`
/// fields are not looked through; one explicit `unwrap()`/`?` does unwrap.
/// Control: drop the `Wrapper::Plain` filter on the receiver in
/// `Bodies::receiver_call` (`o.opt.m()` links to `Inner::m`).
#[test]
fn option_rc_and_vec_fields_leave_their_receiver_untyped() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
use std::rc::Rc;
pub struct Inner;
impl Inner { pub fn m(&self) {} }
pub struct O { opt: Option<Inner>, rc: Rc<Inner>, v: Vec<Inner> }
pub fn wrapped(o: O) { o.opt.m(); o.rc.m(); o.v.m(); }
pub fn unwrapped(o: O) -> Option<()> { o.opt.unwrap().m(); o.opt?.m(); None }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(graph.targets(EdgeKind::Calls, "wrapped"), Vec::<String>::new());
    let sites = receiver_sites(&graph, "wrapped", "m");
    assert_eq!(sites.len(), 3, "{sites:#?}");
    assert!(sites.iter().all(|site| site.replaces.is_none()), "{sites:#?}");
    assert_eq!(graph.targets(EdgeKind::Calls, "unwrapped"), vec!["Inner::m"]);
    let sites = receiver_sites(&graph, "unwrapped", "m");
    assert_eq!(sites.len(), 2, "{sites:#?}");
    assert!(sites.iter().all(|site| site.replaces.is_some()), "{sites:#?}");
}

/// GM-488 N2: a field of a struct declared in another file has no type here,
/// even though the owner itself is typed through the import. No control:
/// it pins the same-file rule.
#[test]
fn a_field_of_a_struct_declared_in_another_file_types_nothing() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod a;\npub mod b;\n"),
        (
            "src/a.rs",
            "pub struct Inner;\nimpl Inner { pub fn m(&self) {} }\npub struct Outer { pub inner: Inner }\n",
        ),
        ("src/b.rs", "use crate::a::Outer;\npub fn run(o: Outer) { o.inner.m(); }\n"),
    ]);
    let graph = krate.extract("src/b.rs");
    assert_eq!(graph.targets(EdgeKind::Calls, "b::run"), Vec::<String>::new());
    let sites = receiver_sites(&graph, "b::run", "m");
    assert_eq!(sites.len(), 1, "{sites:#?}");
    assert_eq!(sites[0].replaces, None);
}

/// GM-488 N3 (D3): a field whose type mentions its struct's generic
/// parameter has no type, though a real `struct T` sits beside it. Control:
/// drop the generic refusal in `Declarer::field_type` (`g.t.m()` and
/// `g.o.unwrap().m()` link to `T::m`).
#[test]
fn a_field_typed_by_a_struct_generic_types_nothing() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub struct T;
impl T { pub fn m(&self) {} }
pub struct G<T> { t: T, o: Option<T> }
pub fn run(g: G<u8>) { g.t.m(); g.o.unwrap().m(); }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    let targets = graph.targets(EdgeKind::Calls, "run");
    assert!(!targets.contains(&"T::m".to_string()), "{targets:?}");
    let sites = receiver_sites(&graph, "run", "m");
    assert_eq!(sites.len(), 2, "{sites:#?}");
    assert!(sites.iter().all(|site| site.replaces.is_none()), "{sites:#?}");
}

/// GM-488 N4: `self` in a trait's default body is any implementor, so
/// `self.f` has no type. The fixture gives the trait a same-named struct
/// (not valid Rust, but the only way the trait's own name could find a
/// field). Control: let `Bodies::field_type` accept `Family::TraitDecl`
/// blocks (`self.f.m()` links to `P::m`).
#[test]
fn a_field_of_self_in_a_trait_default_body_types_nothing() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub struct P;
impl P { pub fn m(&self) {} }
pub struct Tr { f: P }
pub trait Tr { fn d(&self) { self.f.m(); } }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    let d = graph.0.nodes.iter().find(|node| node.name == "d").unwrap().id.clone();
    assert!(
        graph.edges(EdgeKind::Calls).iter().all(|edge| edge.from_id != d),
        "{:#?}",
        graph.edges(EdgeKind::Calls)
    );
    let sites: Vec<_> =
        graph.0.open_sites.iter().filter(|site| site.from_id == d && site.name == "m").collect();
    assert_eq!(sites.len(), 1, "{sites:#?}");
    assert_eq!(sites[0].replaces, None);
}

/// GM-488 N5: `cfg` alternatives of one struct whose field types disagree
/// leave the field untyped, and so does an alternative whose field type no
/// receiver could use (a tuple), in either order; alternatives that agree
/// keep it. Controls: make `FileModel::set_field_type` keep the first type
/// (`S`'s and `U`'s calls link); separately, make it ignore a `None` input
/// (`U`'s and `V`'s calls link).
#[test]
fn cfg_alternatives_that_disagree_on_a_field_type_leave_it_untyped() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub struct P;
impl P { pub fn m(&self) {} }
pub struct Q;
impl Q { pub fn m(&self) {} }
#[cfg(a)] pub struct S { f: P }
#[cfg(not(a))] pub struct S { f: Q }
#[cfg(a)] pub struct U { f: P }
#[cfg(not(a))] pub struct U { f: (P, P) }
#[cfg(a)] pub struct V { f: (P, P) }
#[cfg(not(a))] pub struct V { f: P }
#[cfg(a)] pub struct K { f: P }
#[cfg(not(a))] pub struct K { f: P }
pub fn disagree(s: S) { s.f.m(); }
pub fn unusable_last(u: U) { u.f.m(); }
pub fn unusable_first(v: V) { v.f.m(); }
pub fn agree(k: K) { k.f.m(); }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    for function in ["disagree", "unusable_last", "unusable_first"] {
        assert_eq!(graph.targets(EdgeKind::Calls, function), Vec::<String>::new(), "{function}");
        let sites = receiver_sites(&graph, function, "m");
        assert_eq!(sites.len(), 1, "{function}: {sites:#?}");
        assert_eq!(sites[0].replaces, None, "{function}");
    }
    assert_eq!(graph.targets(EdgeKind::Calls, "agree"), vec!["P::m"]);
}

/// GM-488 N6: a chain through a method this file does not declare with a
/// written return type (a derived `clone`) stops there. No control:
/// `call_type` already requires a same-file `Bound::Here`.
#[test]
fn a_chain_through_a_derived_method_leaves_the_call_untyped() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
#[derive(Clone)]
pub struct Inner;
impl Inner { pub fn m(&self) {} }
pub struct Outer { inner: Inner }
pub fn run(x: Outer) { x.inner.clone().m(); }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    let targets = graph.targets(EdgeKind::Calls, "run");
    assert!(!targets.contains(&"Inner::m".to_string()), "{targets:?}");
    let sites = receiver_sites(&graph, "run", "m");
    assert_eq!(sites.len(), 1, "{sites:#?}");
    assert_eq!(sites[0].replaces, None);
}

/// GM-488 N7: through a field, as through a local, a trait-impl method is a
/// placeholder at the inherent address `Inner::t`, never a link to
/// `<Inner as Tr>::t`. Control: as
/// `a_typed_receiver_finds_the_inherent_method_and_misses_a_trait_impl_method`.
#[test]
fn a_trait_impl_method_through_a_field_is_a_placeholder() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub trait Tr { fn t(&self); }
pub struct Inner;
impl Tr for Inner { fn t(&self) {} }
pub struct Outer { inner: Inner }
pub fn run(o: Outer) { o.inner.t(); }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(graph.targets(EdgeKind::Calls, "run"), vec!["pending_symbol krate::Inner::t"]);
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

/// A trait-impl method points at the trait method it implements, so core's
/// `declared` mode can name it on the method's caller page: one edge per
/// impl, `<Circle as Loud>::speak` to `Loud::speak` only, never to the
/// same-named `Quiet::speak`. Both ends are this file's, so it is resolved;
/// the type-level edges are unchanged. Control: drop the
/// `member_supertypes` call from `impl_item`.
#[test]
fn a_trait_impl_method_is_a_supertype_edge_to_the_trait_method() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub trait Loud { fn speak(&self); }\npub trait Quiet { fn speak(&self); }\npub struct Circle;\n\
         impl Loud for Circle { fn speak(&self) {} }\nimpl Quiet for Circle { fn speak(&self) {} }\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(graph.targets(EdgeKind::SupertypeOf, "<Circle as Loud>::speak"), vec!["Loud::speak"]);
    assert_eq!(graph.targets(EdgeKind::SupertypeOf, "<Circle as Quiet>::speak"), vec!["Quiet::speak"]);
    assert_eq!(graph.targets(EdgeKind::SupertypeOf, "Circle"), vec!["Loud", "Quiet"]);
    assert!(graph.edges(EdgeKind::SupertypeOf).iter().all(|edge| edge.resolved));
}

/// The trait one file over: the method's edge is unresolved, onto a
/// placeholder addressing the trait's method by `qualifiedName` in the
/// trait's module - the address a written `Shape::area` path gets - for the
/// linker to land on the declaration.
#[test]
fn a_trait_impl_method_of_a_trait_in_another_file_points_at_a_placeholder() {
    let krate = Crate::new(&[
        (
            "src/lib.rs",
            "pub mod shapes;\nuse crate::shapes::Shape;\npub struct Ci;\nimpl Shape for Ci { fn area(&self) {} }\n",
        ),
        ("src/shapes.rs", "pub trait Shape { fn area(&self); }\n"),
    ]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(
        graph.targets(EdgeKind::SupertypeOf, "<Ci as Shape>::area"),
        vec!["pending_symbol krate::shapes::shapes::Shape::area"]
    );
    let placeholder = graph.placeholder("pending_symbol", "area");
    assert_eq!(
        graph.target_of(placeholder),
        (container("krate::shapes"), TargetKey::QualifiedName("shapes::Shape::area".into()))
    );
}

/// The `SUPERTYPE_OF` edges out of a `Function` node: none of them is what
/// the declared member edge's absence means.
fn member_supertype_edges(graph: &Graph) -> Vec<String> {
    graph
        .edges(EdgeKind::SupertypeOf)
        .into_iter()
        .filter(|edge| graph.by_id(&edge.from_id).kind == NodeKind::Function)
        .map(|edge| {
            format!(
                "{} -> {}",
                graph.by_id(&edge.from_id).qualified_name,
                graph.by_id(&edge.to_id).qualified_name
            )
        })
        .collect()
}

/// An inherent method implements nothing, though its type implements a
/// trait with a same-named method. Control: call `member_supertypes` for
/// the inherent arm of `impl_item` too.
#[test]
fn an_inherent_method_is_no_supertype_edge() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub trait Shape { fn area(&self); }\npub struct Square;\nimpl Square { fn area(&self) {} }\n\
         impl Shape for Square { fn area(&self) {} }\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    assert!(graph.targets(EdgeKind::SupertypeOf, "Square::area").is_empty());
    assert_eq!(member_supertype_edges(&graph), vec!["<Square as Shape>::area -> Shape::area"]);
}

/// A trait outside the project (`Clone`, `std::fmt::Display`) has no method
/// node to point at: no member edge and no placeholder for one. Control:
/// drop the `Here`/`There` gate in `member_supertypes`.
#[test]
fn a_method_of_an_external_trait_is_no_supertype_edge() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub struct T;\nimpl Clone for T { fn clone(&self) -> T { T } }\n\
         impl std::fmt::Display for T { fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { Ok(()) } }\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    assert!(member_supertype_edges(&graph).is_empty(), "{:#?}", member_supertype_edges(&graph));
    assert!(
        !graph.0.nodes.iter().any(|node| node.native_kind.as_deref() == Some("pending_symbol")
            && (node.name == "clone" || node.name == "fmt")),
        "{:#?}",
        graph.names()
    );
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
/// beside the inherent methods; a tuple struct's positional fields are nodes
/// `T.0`, `T.1` (GM-528), enum-variant fields are not. Its uses are references: `self.f` in the impl and a literal's or
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
    assert_eq!(fields.len(), 5, "Ledger's named fields and Pair's two: {:#?}", store.names());

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
    // semantic tier, at the field name; `ledger` is typed by its literal, so
    // the site replaces the structural edge onto the field.
    let reads: Vec<_> = user
        .0
        .open_sites
        .iter()
        .filter(|site| site.kind == OpenSiteKind::ReceiverField && site.name == "all_unresolved")
        .collect();
    assert_eq!(reads.len(), 1, "{:#?}", user.0.open_sites);
    assert_eq!(reads[0].edge_kind, EdgeKind::References);
    assert!(reads[0].replaces.is_some(), "{:#?}", reads[0]);
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

// --- GM-497: typed field reads as receiver-field open sites -------------------

/// The `ReceiverField` open sites named `name` out of `from`, in source order.
fn field_sites<'g>(graph: &'g Graph, from: &str, name: &str) -> Vec<&'g OpenSite> {
    let from = graph.node(from).id.clone();
    graph
        .0
        .open_sites
        .iter()
        .filter(|site| site.kind == OpenSiteKind::ReceiverField && site.from_id == from && site.name == name)
        .collect()
}

/// The one `REFERENCES` edge out of `from` onto the node `to` (a
/// declaration's qualified name, or a placeholder's).
fn reference_onto<'g>(graph: &'g Graph, from: &str, to: &str) -> &'g WireEdge {
    let from = graph.node(from).id.clone();
    let found: Vec<_> = graph
        .edges(EdgeKind::References)
        .into_iter()
        .filter(|edge| edge.from_id == from && graph.by_id(&edge.to_id).qualified_name == to)
        .collect();
    assert_eq!(found.len(), 1, "one edge {from} -> {to}: {found:#?}");
    found[0]
}

/// GM-497 items 1 and 9: `x.f` on a value typed to a same-file struct links
/// `T.f` with a resolved `REFERENCES` edge, and keeps one `ReceiverField`
/// site at the field token that names that edge in `replaces`. Control:
/// make `Bodies::field_access` pass `replaces: None` and emit no edge (no
/// `T.f` target, `replaces` is `None`).
#[test]
fn a_typed_field_read_links_the_field_and_keeps_a_site_that_replaces_the_edge() {
    let krate = Crate::new(&[("src/lib.rs", "pub struct T { pub f: u8 }\npub fn run(x: T) -> u8 { x.f }\n")]);
    let graph = krate.extract("src/lib.rs");
    assert!(graph.targets(EdgeKind::References, "run").contains(&"T.f".to_string()));
    let edge = reference_onto(&graph, "run", "T.f");
    assert!(edge.resolved, "a same-file field is a resolved edge");
    let sites = field_sites(&graph, "run", "f");
    assert_eq!(sites.len(), 1, "{:#?}", graph.0.open_sites);
    let site = sites[0];
    assert_eq!(site.edge_kind, EdgeKind::References);
    assert_eq!(site.from_container.as_deref(), Some("krate"));
    assert_eq!(site.replaces.as_deref(), Some(edge.id.as_str()), "the site names the edge it replaces");
    // The position is the field name's token, not the receiver's.
    let line = "pub fn run(x: T) -> u8 { x.f }";
    assert_eq!((site.position.line, site.position.col as usize), (1, line.find("x.f").unwrap() + 2));
}

/// GM-497 item 2: the struct is declared in another file, so the edge is
/// onto a `pending_symbol` placeholder addressed `(Container(T's module),
/// QualifiedName("…T.f"))`, and the site replaces that edge. Control: as
/// above (no placeholder edge, `replaces` is `None`).
#[test]
fn a_field_read_on_a_type_from_another_file_replaces_a_placeholder_edge() {
    let krate = Crate::new(&[
        ("src/lib.rs", "pub mod store;\npub mod user;\n"),
        ("src/store.rs", "pub struct T { pub f: u8 }\n"),
        ("src/user.rs", "use crate::store::T;\npub fn run(x: T) -> u8 { x.f }\n"),
    ]);
    let user = krate.extract("src/user.rs");
    let placeholder = user.placeholder("pending_symbol", "f");
    assert_eq!(
        user.target_of(placeholder),
        (container("krate::store"), TargetKey::QualifiedName("store::T.f".to_string()))
    );
    let edge = reference_onto(&user, "user::run", &placeholder.qualified_name.clone());
    assert_eq!(edge.to_id, placeholder.id);
    let sites = field_sites(&user, "user::run", "f");
    assert_eq!(sites.len(), 1, "{:#?}", user.0.open_sites);
    assert_eq!(sites[0].replaces.as_deref(), Some(edge.id.as_str()));
}

/// GM-497 item 3: a receiver this tier cannot type - an unknown name, a
/// generic, a trait object, a wrapper (`Option<T>`), a chain past the hop
/// budget - gets no edge onto a field named `f`, and a `ReceiverField` site
/// that replaces nothing. `beyond` also pins that a field read has no hop
/// budget of its own: it reads at the depth where `F::three_fields` in
/// `fields_and_calls_share_the_two_hop_budget` stops typing its call.
/// Control: drop the `Wrapper::Plain` filter in `field_access` (`wrapped`
/// links `T.f`); give `field_access` an extra hop (`beyond` links `D.k`).
#[test]
fn an_untyped_field_read_links_nothing_and_keeps_a_site_that_replaces_nothing() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub trait Tr {}
pub struct T { pub f: u8 }
pub fn unknown() { y.f; }
pub fn generic<G: Tr>(x: G) { x.f; }
pub fn object(x: &dyn Tr) { x.f; }
pub fn wrapped(x: Option<T>) { x.f; }
pub struct D { pub k: u8 }
pub struct C { pub h: D }
pub struct B { pub g: C }
pub struct F { pub f: B }
impl F {
    pub fn within(&self) { self.f.g.h; }
    pub fn beyond(&self) { self.f.g.h.k; }
}
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    for (from, name) in
        [("unknown", "f"), ("generic", "f"), ("object", "f"), ("wrapped", "f"), ("F::beyond", "k")]
    {
        let targets = graph.targets(EdgeKind::References, from);
        assert!(!targets.iter().any(|target| target.ends_with(&format!(".{name}"))), "{from}: {targets:?}");
        let sites = field_sites(&graph, from, name);
        assert_eq!(sites.len(), 1, "{from}: {:#?}", graph.0.open_sites);
        assert_eq!(sites[0].replaces, None, "{from}");
        assert_eq!(sites[0].edge_kind, EdgeKind::References, "{from}");
    }
    // The budget itself is unchanged: the level below still links.
    assert!(graph.targets(EdgeKind::References, "F::within").contains(&"C.h".to_string()));
    assert!(field_sites(&graph, "F::within", "h")[0].replaces.is_some());
}

/// GM-497 item 4: `x.a.f` with both levels typed gets an edge and a site per
/// level, each site replacing its own edge. Control: `replaces: None` with
/// no edge in `field_access` (neither `X.a` nor `A.f` links).
#[test]
fn each_level_of_a_typed_field_chain_gets_its_own_edge_and_site() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub struct A { pub f: u8 }\npub struct X { pub a: A }\npub fn run(x: X) -> u8 { x.a.f }\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    for (field, target) in [("a", "X.a"), ("f", "A.f")] {
        let edge = reference_onto(&graph, "run", target);
        let sites = field_sites(&graph, "run", field);
        assert_eq!(sites.len(), 1, "{field}: {:#?}", graph.0.open_sites);
        assert_eq!(sites[0].replaces.as_deref(), Some(edge.id.as_str()), "{field}");
    }
}

/// GM-497 item 5: a positional field `x.0` opens no site of its own (since
/// GM-528 a typed one links `P.0` instead), and its value is still visited
/// (`x.a` links). Control: drop the
/// `field_identifier` filter in `field_access` (a site named `0` appears).
#[test]
fn a_positional_field_read_emits_nothing_but_its_value_is_visited() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub struct P(pub u8);\npub struct X { pub a: P }\npub fn run(x: X) -> u8 { x.a.0 }\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    assert!(graph.targets(EdgeKind::References, "run").contains(&"X.a".to_string()));
    assert_eq!(field_sites(&graph, "run", "a").len(), 1);
    let run = graph.node("run").id.clone();
    let positional: Vec<_> =
        graph.0.open_sites.iter().filter(|site| site.from_id == run && site.name == "0").collect();
    assert!(positional.is_empty(), "{positional:#?}");
}

/// GM-497 item 6: `self.f` inside `impl T` links `T.f` and opens no site at
/// all. Control: route the `self` branch of `field_access` through the
/// typed path (a `ReceiverField` site named `f` appears in `T::get`).
#[test]
fn a_self_field_read_links_the_field_and_opens_no_site() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        "pub struct T { pub f: u8 }\nimpl T { pub fn get(&self) -> u8 { self.f } }\n",
    )]);
    let graph = krate.extract("src/lib.rs");
    assert_eq!(graph.targets(EdgeKind::References, "T::get"), vec!["T.f"]);
    let get = graph.node("T::get").id.clone();
    let sites: Vec<_> = graph.0.open_sites.iter().filter(|site| site.from_id == get).collect();
    assert!(sites.is_empty(), "{sites:#?}");
}

/// GM-497 items 7 and 8: same-named fields of two structs link each to its
/// own struct, and a field read never lands on a same-named method. Control:
/// address the field through `type_address` of the wrong type, or through
/// the method's `T::f` key, in `field_of` (a wrong target appears).
#[test]
fn a_field_read_links_its_own_structs_field_and_never_a_same_named_method() {
    let krate = Crate::new(&[(
        "src/lib.rs",
        r#"
pub struct A { pub f: u8 }
pub struct B { pub f: u8 }
impl B { pub fn f(&self) -> u8 { 0 } }
pub fn read_a(a: A) -> u8 { a.f }
pub fn read_b(b: B) -> u8 { b.f }
"#,
    )]);
    let graph = krate.extract("src/lib.rs");
    let fields = |from: &str| -> Vec<String> {
        graph.targets(EdgeKind::References, from).into_iter().filter(|target| target.ends_with("f")).collect()
    };
    assert_eq!(fields("read_a"), vec!["A.f"]);
    assert_eq!(fields("read_b"), vec!["B.f"]);
    assert_eq!(graph.targets(EdgeKind::Calls, "read_b"), Vec::<String>::new(), "a field read is not a call");
    for (from, target) in [("read_a", "A.f"), ("read_b", "B.f")] {
        let edge = reference_onto(&graph, from, target);
        assert_eq!(field_sites(&graph, from, "f")[0].replaces.as_deref(), Some(edge.id.as_str()), "{from}");
    }
}

// --- the whole-file range (GM-527) ----------------------------------------------

/// The `File` node ends where the file's content ends - trailing whitespace
/// trimmed, then `(newlines, chars of the last line)` - never on the empty
/// line after a final newline.
///
/// Control: restore the old body of `Positions::file_range` (emit.rs:
/// `(split('\n').count() - 1, chars after the last '\n')`); every row ending
/// in whitespace fails.
#[test]
fn the_file_node_ends_on_the_files_last_real_line() {
    let cases: &[(&str, &str, (u32, u32))] = &[
        ("empty", "", (0, 0)),
        ("no final newline", "fn a() {}\nfn bc() {}", (1, 10)),
        ("a final newline", "fn a() {}\nfn bc() {}\n", (1, 10)),
        ("ends in two newlines", "fn a() {}\nfn bc() {}\n\n", (1, 10)),
        ("trailing spaces", "fn a() {}\nfn bc() {}  \n", (1, 10)),
        ("chars, not bytes", "fn a() {}\nconst E: &str = \"é😀\";\n", (1, 21)),
    ];
    for (what, source, (line, col)) in cases {
        let krate = Crate::new(&[("src/lib.rs", source)]);
        let graph = krate.extract("src/lib.rs");
        let file = graph.node("src/lib.rs");
        assert_eq!(file.kind, NodeKind::File, "{what}");
        assert_eq!((file.range.start.line, file.range.start.col), (0, 0), "{what}: {source:?}");
        assert_eq!((file.range.end.line, file.range.end.col), (*line, *col), "{what}: {source:?}");
    }
}
