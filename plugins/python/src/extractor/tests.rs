//! Unit tests for the extractor, one per shape the task names.
//!
//! Every one of them runs against a **real** [`ProjectContext`] built from a
//! real (temporary) package tree on disk, rather than against
//! `ProjectContext::default()`. That is not thoroughness for its own sake:
//! the default model has no roots at all, so it places every file as an
//! *orphan* (`crate::project`, Decision 4/5), whose container key is
//! `orphan:<path>`, whose `parent` is `None` and whose relative imports
//! resolve to nothing. Every container key, every relative import and every
//! self-announcement would then be tested in the one configuration where they
//! are all degenerate, and a suite that passed would say nothing about the
//! case the plugin actually runs in.

use std::path::PathBuf;

use g_mesh_plugin_sdk::wire::{EdgeKind, NodeKind, TargetKey, TargetScope, Visibility, WireEdge, WireNode};
use g_mesh_plugin_sdk::{Extractor, FileGraph, OpenSiteKind, RelPath};

use super::PythonExtractor;
use crate::project::ProjectContext;

/// A temporary package tree, removed on drop.
struct Tree {
    root: PathBuf,
    project: ProjectContext,
}

impl Tree {
    /// A project whose files are `files`, each a `(path, contents)` pair.
    fn new(files: &[(&str, &str)]) -> Self {
        // A process-wide counter, not a timestamp: `cargo test` runs these
        // concurrently in one process, and two trees that share a path race on
        // `create_dir_all`/`remove_dir_all` - which shows up as a *later* test
        // seeing a tree another test has just deleted, i.e. as an assertion
        // about containers failing for no reason connected to containers.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("g-mesh-plugin-python-extract-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
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
        Graph(PythonExtractor.extract(&self.project, &RelPath::new(path), &source))
    }
}

impl Drop for Tree {
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
        self.0.nodes.iter().map(|node| (node.qualified_name.clone(), node.native_kind.clone())).collect()
    }

    fn edges(&self, kind: EdgeKind) -> Vec<&WireEdge> {
        self.0.edges.iter().filter(|edge| edge.kind == kind).collect()
    }

    /// The nodes an edge of `kind` out of `from` lands on, rendered as
    /// `qualifiedName` (a declaration) or `<nativeKind> <qualifiedName>` (a
    /// placeholder, whose qualified name is the address it is waiting on).
    fn targets(&self, kind: EdgeKind, from: &str) -> Vec<String> {
        let from = self.node(from).id.clone();
        let mut targets: Vec<String> = self
            .edges(kind)
            .into_iter()
            .filter(|edge| edge.from_id == from)
            .map(|edge| {
                let node = self.by_id(&edge.to_id);
                // Only the four placeholder kinds are prefixed: a real
                // declaration is named by its `qualifiedName` alone, and the
                // module self-announcement (`nativeKind = "module"`) is a
                // real declaration, not a placeholder.
                match node.native_kind.as_deref() {
                    Some(
                        native @ ("pending_symbol" | "reexport" | "resolved_module" | "external_module"),
                    ) => {
                        format!("{native} {}", node.qualified_name)
                    }
                    _ => node.qualified_name.clone(),
                }
            })
            .collect();
        targets.sort();
        targets
    }

    /// The `(edge kind, resolved)` of the one edge between two nodes named by
    /// `qualifiedName`, for a test that cares which it is.
    fn edge_between(&self, from: &str, to: &str) -> Option<(EdgeKind, bool)> {
        let from = self.node(from).id.clone();
        let to = self.node(to).id.clone();
        self.0
            .edges
            .iter()
            .find(|edge| edge.from_id == from && edge.to_id == to)
            .map(|edge| (edge.kind, edge.resolved))
    }

    fn open_site_names(&self) -> Vec<(&str, OpenSiteKind)> {
        self.0.open_sites.iter().map(|site| (site.name.as_str(), site.kind)).collect()
    }

    fn placeholder_target(&self, qualified_name: &str) -> (String, String, String) {
        let node = self.node(qualified_name);
        let target = node.target.as_ref().expect("a placeholder carries its target");
        let scope = match &target.scope {
            TargetScope::Container(container) => format!("container:{container}"),
            TargetScope::File(path) => format!("file:{path}"),
        };
        let key = match &target.key {
            TargetKey::Name(name) => format!("name:{name}"),
            TargetKey::QualifiedName(qualified) => format!("qualifiedName:{qualified}"),
        };
        (scope, key, target.from_container.clone().unwrap_or_default())
    }
}

/// The package tree most tests use: a `pkg` with a subpackage two deep, so
/// that container keys, parents and relative imports are all non-degenerate.
fn tree(files: &[(&str, &str)]) -> Tree {
    let mut all: Vec<(&str, &str)> = vec![
        ("pkg/__init__.py", ""),
        ("pkg/sub/__init__.py", ""),
        ("pkg/base.py", "class Base:\n    def describe(self):\n        return 1\n"),
    ];
    all.extend_from_slice(files);
    Tree::new(&all)
}

// --- declarations -------------------------------------------------------------

/// The whole `nativeKind` table of `super::decls`, in one file.
#[test]
fn every_declaration_shape_gets_its_kind_and_its_lexical_path() {
    let tree = tree(&[(
        "pkg/mod.py",
        "CONST = 1\n\
         \n\
         def f():\n\
         \x20   def inner():\n\
         \x20       pass\n\
         \n\
         async def a():\n\
         \x20   pass\n\
         \n\
         class C:\n\
         \x20   attr = 2\n\
         \n\
         \x20   def m(self):\n\
         \x20       pass\n\
         \n\
         \x20   async def am(self):\n\
         \x20       pass\n",
    )]);
    let graph = tree.extract("pkg/mod.py");
    let shapes: Vec<(&str, NodeKind, &str)> = vec![
        ("CONST", NodeKind::Variable, "variable"),
        ("f", NodeKind::Function, "function"),
        ("f.inner", NodeKind::Function, "function"),
        ("a", NodeKind::Function, "function"),
        ("C", NodeKind::Type, "class"),
        ("C.m", NodeKind::Function, "method"),
        ("C.am", NodeKind::Function, "method"),
    ];
    for (qualified, kind, native) in shapes {
        let node = graph.node(qualified);
        assert_eq!(node.kind, kind, "{qualified}");
        assert_eq!(node.native_kind.as_deref(), Some(native), "{qualified}");
        assert_eq!(node.container.as_deref(), Some("pkg.mod"), "{qualified}");
        assert_eq!(node.container_parent.as_deref(), Some("pkg"), "{qualified}");
    }
    // A class-body assignment is deliberately not a node - see `decls`.
    assert!(graph.find("C.attr").is_none(), "{:#?}", graph.names());
}

/// The decision the task asks to be argued: a method of a nested class
/// carries **both** class names - see `super::keys`, Decision 2.
#[test]
fn a_method_of_a_nested_class_is_named_by_its_whole_lexical_path() {
    let tree = tree(&[(
        "pkg/mod.py",
        "class Request:\n\
         \x20   class Inner:\n\
         \x20       def run(self):\n\
         \x20           pass\n\
         \n\
         class Response:\n\
         \x20   class Inner:\n\
         \x20       def run(self):\n\
         \x20           pass\n",
    )]);
    let graph = tree.extract("pkg/mod.py");
    // The point: the two `Inner.run`s are two nodes, not one. Under the
    // rejected `Inner.run` scheme they would share an id and the second would
    // silently replace the first.
    let first = graph.node("Request.Inner.run");
    let second = graph.node("Response.Inner.run");
    assert_ne!(first.id, second.id);
    assert_eq!(first.name, "run");
    assert_eq!(second.name, "run");
    assert!(graph.find("Request.Inner").is_some());
    assert!(graph.find("Response.Inner").is_some());
}

/// `async` lives in the signature, never in the id - see `super::decls`.
#[test]
fn adding_async_keeps_a_functions_id_and_changes_only_its_signature() {
    let tree =
        tree(&[("pkg/mod.py", "def f():\n    pass\n"), ("pkg/other.py", "async def f():\n    pass\n")]);
    let plain = tree.extract("pkg/mod.py");
    let asynchronous = tree.extract("pkg/other.py");
    assert_eq!(plain.node("f").native_kind, asynchronous.node("f").native_kind);
    assert_eq!(plain.node("f").signature.as_deref(), Some("def f()"));
    assert_eq!(asynchronous.node("f").signature.as_deref(), Some("async def f()"));
}

#[test]
fn decorators_are_in_the_signature_and_are_references_of_their_own() {
    let tree = tree(&[(
        "pkg/mod.py",
        "def register(fn):\n\
         \x20   return fn\n\
         \n\
         @register\n\
         def handler(request):\n\
         \x20   pass\n",
    )]);
    let graph = tree.extract("pkg/mod.py");
    assert_eq!(graph.node("handler").signature.as_deref(), Some("@register def handler(request)"));
    // The decorator is a use of `register`, attributed to what it decorates.
    assert_eq!(graph.edge_between("handler", "register"), Some((EdgeKind::References, true)));
}

/// Python has no doc-comment syntax: the docstring is the first *statement*,
/// and a `#` comment documents nothing the index carries.
#[test]
fn a_docstring_is_the_doc_comment_and_a_hash_comment_is_not() {
    let tree = tree(&[(
        "pkg/mod.py",
        "\"\"\"Module doc.\"\"\"\n\
         \n\
         # not documentation\n\
         def f():\n\
         \x20   \"\"\"Doc of f.\"\"\"\n\
         \n\
         def g():\n\
         \x20   pass\n",
    )]);
    let graph = tree.extract("pkg/mod.py");
    assert_eq!(graph.node("pkg/mod.py").doc_comment.as_deref(), Some("Module doc."));
    assert_eq!(graph.node("pkg.mod").doc_comment.as_deref(), Some("Module doc."));
    assert_eq!(graph.node("f").doc_comment.as_deref(), Some("Doc of f."));
    assert_eq!(graph.node("g").doc_comment, None);
}

/// Decision 3: Python enforces nothing, so nothing is narrower than
/// `public` - underscore or not.
#[test]
fn every_declaration_is_public_and_exported_including_an_underscored_one() {
    let tree = tree(&[("pkg/mod.py", "def _helper():\n    pass\n\ndef visible():\n    pass\n")]);
    let graph = tree.extract("pkg/mod.py");
    for name in ["_helper", "visible"] {
        assert_eq!(graph.node(name).visibility, Visibility::Public, "{name}");
    }
    let exported = graph.targets(EdgeKind::Exports, "pkg/mod.py");
    assert!(exported.contains(&"_helper".to_string()), "{exported:?}");
    assert!(exported.contains(&"visible".to_string()), "{exported:?}");
}

// --- the self-announcement node (crate::project, Decisions 1 and 2) -----------

#[test]
fn a_module_announces_itself_as_a_member_of_its_package() {
    let tree = tree(&[("pkg/sub/deep.py", "def f():\n    pass\n")]);
    let graph = tree.extract("pkg/sub/deep.py");
    let announcement = graph.node("pkg.sub.deep");
    assert_eq!(announcement.kind, NodeKind::Module);
    assert_eq!(announcement.native_kind.as_deref(), Some("module"));
    assert_eq!(announcement.name, "deep", "the name a `from pkg.sub import deep` looks for");
    assert_eq!(announcement.container.as_deref(), Some("pkg.sub"));
    assert_eq!(announcement.container_parent.as_deref(), Some("pkg"));
    assert_eq!(announcement.visibility, Visibility::Public);
    // ...and it is a `DEFINES` target of the file, like any other declaration.
    assert!(graph.targets(EdgeKind::Defines, "pkg/sub/deep.py").contains(&"pkg.sub.deep".to_string()));
}

#[test]
fn a_package_init_announces_the_package_itself() {
    let tree = tree(&[("pkg/sub/__init__.py", "def f():\n    pass\n")]);
    let graph = tree.extract("pkg/sub/__init__.py");
    let announcement = graph.node("pkg.sub");
    assert_eq!(announcement.native_kind.as_deref(), Some("package"));
    assert_eq!(announcement.name, "sub");
    assert_eq!(announcement.container.as_deref(), Some("pkg"));
    // Decision 2: the `__init__`'s own declarations are members of the
    // package, not of a `pkg.sub.__init__` of their own.
    assert_eq!(graph.node("f").container.as_deref(), Some("pkg.sub"));
}

/// The other direction of the acceptance criterion: a `.pyi` stub announces
/// nothing, and in fact contributes nothing at all beyond its `File` node -
/// `crate::project`'s Decision 6 and `super`'s Decision 8.
#[test]
fn a_pyi_stub_contributes_its_file_node_and_nothing_else() {
    let tree = tree(&[
        ("pkg/mod.py", "def greet(name):\n    return name\n"),
        ("pkg/mod.pyi", "from pkg.base import Base\n\ndef greet(name: str) -> str: ...\n"),
    ]);
    let module = tree.extract("pkg/mod.py");
    assert!(module.find("pkg.mod").is_some(), "the module does announce itself");

    let stub = tree.extract("pkg/mod.pyi");
    assert_eq!(stub.0.nodes.len(), 1, "{:#?}", stub.names());
    assert_eq!(stub.0.nodes[0].kind, NodeKind::File);
    assert_eq!(stub.0.nodes[0].qualified_name, "pkg/mod.pyi");
    assert!(stub.find("pkg.mod").is_none(), "a stub must never announce its sibling's name");
    assert!(stub.find("greet").is_none(), "a stub declares nothing");
    assert!(stub.0.edges.is_empty(), "{:#?}", stub.0.edges);
    assert!(stub.0.open_sites.is_empty());
}

/// A module with no package above it has no container to be a member of -
/// see `ModuleCtx::announcement`'s own doc for why nothing is lost.
#[test]
fn a_top_level_module_announces_nothing() {
    let tree = Tree::new(&[("script.py", "def f():\n    pass\n")]);
    let graph = tree.extract("script.py");
    assert!(graph.find("script").is_none(), "{:#?}", graph.names());
    assert_eq!(graph.node("f").container.as_deref(), Some("script"));
    assert_eq!(graph.node("f").container_parent, None);
}

// --- same-file edges ----------------------------------------------------------

#[test]
fn a_same_file_call_and_reference_are_direct_and_resolved() {
    let tree = tree(&[(
        "pkg/mod.py",
        "GREETING = \"hi\"\n\
         \n\
         def greet(name):\n\
         \x20   return decorate(GREETING, name)\n\
         \n\
         def decorate(prefix, name):\n\
         \x20   return prefix + name\n",
    )]);
    let graph = tree.extract("pkg/mod.py");
    // Forward reference: `decorate` is declared *below* the call, which is
    // exactly why the extractor runs two passes.
    assert_eq!(graph.edge_between("greet", "decorate"), Some((EdgeKind::Calls, true)));
    assert_eq!(graph.edge_between("greet", "GREETING"), Some((EdgeKind::References, true)));
}

/// A constructor call is a *use* of a class: core's linker lands a `CALLS`
/// edge only on a `Function`.
#[test]
fn calling_a_class_is_a_reference_not_a_call() {
    let tree = tree(&[(
        "pkg/mod.py",
        "class Greeter:\n\
         \x20   pass\n\
         \n\
         def build():\n\
         \x20   return Greeter()\n",
    )]);
    let graph = tree.extract("pkg/mod.py");
    assert_eq!(graph.edge_between("build", "Greeter"), Some((EdgeKind::References, true)));
}

/// Python's own scoping, twice over: a method body does not see its class's
/// names, and a nested function's name does.
#[test]
fn a_bare_name_resolves_the_way_python_resolves_it() {
    let tree = tree(&[(
        "pkg/mod.py",
        "def greet(name):\n\
         \x20   return name\n\
         \n\
         class C:\n\
         \x20   def helper(self):\n\
         \x20       pass\n\
         \n\
         \x20   def run(self):\n\
         \x20       return greet(helper)\n\
         \n\
         def outer():\n\
         \x20   def inner():\n\
         \x20       pass\n\
         \x20   return inner()\n",
    )]);
    let graph = tree.extract("pkg/mod.py");
    // The module-level `greet` is in scope inside a method...
    assert_eq!(graph.edge_between("C.run", "greet"), Some((EdgeKind::Calls, true)));
    // ...and the sibling method `C.helper` is not: a bare `helper` inside a
    // method is a `NameError`, so linking it would report a use Python raises
    // on.
    assert_eq!(graph.edge_between("C.run", "C.helper"), None);
    // A nested definition is a declaration of the enclosing function's scope.
    assert_eq!(graph.edge_between("outer", "outer.inner"), Some((EdgeKind::Calls, true)));
}

#[test]
fn a_local_binding_never_becomes_an_edge() {
    let tree = tree(&[(
        "pkg/mod.py",
        "def parse(text):\n\
         \x20   return text\n\
         \n\
         def run(config):\n\
         \x20   parse = config\n\
         \x20   for parse in config:\n\
         \x20       pass\n\
         \x20   return parse\n",
    )]);
    let graph = tree.extract("pkg/mod.py");
    assert_eq!(graph.edge_between("run", "parse"), None, "a local shadows the module-level `parse`");
}

// --- imports ------------------------------------------------------------------

#[test]
fn a_plain_and_an_aliased_module_import_are_both_container_imports() {
    let tree = tree(&[
        ("pkg/helpers.py", "def assist():\n    pass\n"),
        (
            "pkg/mod.py",
            "import pkg.helpers\n\
             import pkg.base as base_module\n\
             \n\
             def run():\n\
             \x20   return base_module.Base\n",
        ),
    ]);
    let graph = tree.extract("pkg/mod.py");
    let imports = graph.targets(EdgeKind::Imports, "pkg/mod.py");
    assert_eq!(
        imports,
        vec!["resolved_module pkg.base::*".to_string(), "resolved_module pkg.helpers::*".to_string()]
    );
    // The alias binds the *whole* dotted path, so a member of it is addressed
    // in container `pkg.base`.
    assert_eq!(
        graph.placeholder_target("pkg.base::Base"),
        ("container:pkg.base".into(), "name:Base".into(), "pkg.mod".into())
    );
}

#[test]
fn a_from_import_is_a_name_keyed_placeholder_in_the_container_it_names() {
    let tree = tree(&[("pkg/mod.py", "from pkg.base import Base as Parent\n")]);
    let graph = tree.extract("pkg/mod.py");
    assert_eq!(
        graph.placeholder_target("pkg.base::Base"),
        ("container:pkg.base".into(), "name:Base".into(), "pkg.mod".into()),
        "an alias renames the binding, never the address"
    );
    // The import line belongs to the file, so the edge starts there.
    assert_eq!(
        graph.targets(EdgeKind::References, "pkg/mod.py"),
        vec!["pending_symbol pkg.base::Base".to_string()]
    );
}

/// The acceptance case: a relative import **two levels up**, from a module
/// two packages deep.
#[test]
fn a_relative_import_resolves_against_the_modules_own_package() {
    let tree = tree(&[
        (
            "pkg/sub/deep.py",
            "from . import sibling\n\
         from .. import base\n\
         from ..base import Base\n",
        ),
        ("pkg/sub/sibling.py", ""),
    ]);
    let graph = tree.extract("pkg/sub/deep.py");
    // One dot is the module's own package...
    assert_eq!(
        graph.placeholder_target("pkg.sub::sibling"),
        ("container:pkg.sub".into(), "name:sibling".into(), "pkg.sub.deep".into())
    );
    // ...two dots strip one package off it...
    assert_eq!(
        graph.placeholder_target("pkg::base"),
        ("container:pkg".into(), "name:base".into(), "pkg.sub.deep".into())
    );
    // ...and a written tail is appended to whatever the dots reached.
    assert_eq!(
        graph.placeholder_target("pkg.base::Base"),
        ("container:pkg.base".into(), "name:Base".into(), "pkg.sub.deep".into())
    );
}

/// In an `__init__.py` the one dot is the package *itself*, because an
/// `__init__` file is its package - `super::keys`, Decision 4.
#[test]
fn a_relative_import_in_an_init_file_names_the_package_itself() {
    let tree = tree(&[("pkg/sub/__init__.py", "from . import deep\n"), ("pkg/sub/deep.py", "")]);
    let graph = tree.extract("pkg/sub/__init__.py");
    assert_eq!(
        graph.placeholder_target("pkg.sub::deep"),
        ("container:pkg.sub".into(), "name:deep".into(), "pkg.sub".into())
    );
    // GM-358: `deep` is a real submodule of `pkg.sub`, not a symbol declared
    // inside it, so `from . import deep` also loads `pkg.sub.deep` as a side
    // effect - the same shape `src/requests/__init__.py:158`'s
    // `from . import packages, utils` has in psf/requests. The placeholder
    // assertion above (`find_references`'s address) already worked before
    // this fix; this `IMPORTS` edge is what `get_dependencies` needs and did
    // not have.
    assert_eq!(
        graph.targets(EdgeKind::Imports, "pkg/sub/__init__.py"),
        vec!["resolved_module pkg.sub.deep::*".to_string(), "resolved_module pkg.sub::*".to_string()],
        "{:#?}",
        graph.names()
    );
}

/// The other measured GM-358 shape: an *absolute* `from pkg import name`
/// where `name` may be either a submodule or an ordinary symbol, in the same
/// statement family. `sub` must gain the extra `IMPORTS` edge; `assist`,
/// resolved through `pkg.helper`'s own container edge exactly as before this
/// fix, must not - it is the control that shows the fix discriminates rather
/// than firing on every `from` import indiscriminately.
#[test]
fn a_from_import_of_a_submodule_gains_an_imports_edge_a_plain_symbol_import_does_not() {
    let tree = tree(&[
        ("pkg/__init__.py", ""),
        ("pkg/sub.py", ""),
        ("pkg/helper.py", "def assist():\n    pass\n"),
        ("pkg/mod.py", "from pkg import sub\nfrom pkg.helper import assist\n"),
    ]);
    let graph = tree.extract("pkg/mod.py");
    assert_eq!(
        graph.targets(EdgeKind::Imports, "pkg/mod.py"),
        vec![
            "resolved_module pkg.helper::*".to_string(),
            "resolved_module pkg.sub::*".to_string(),
            "resolved_module pkg::*".to_string(),
        ],
        "`sub` (a submodule) gains its own edge; `assist` (a plain symbol) \
         does not gain a phantom one onto `pkg.helper.assist` - {:#?}",
        graph.names()
    );
    // The symbol import's own address is untouched by the fix.
    assert_eq!(
        graph.placeholder_target("pkg.helper::assist"),
        ("container:pkg.helper".into(), "name:assist".into(), "pkg.mod".into())
    );
}

/// Python's own `ImportError: attempted relative import beyond top-level
/// package`, answered with nothing rather than with a guess.
#[test]
fn a_relative_import_above_the_top_level_package_emits_nothing() {
    let tree = tree(&[("pkg/mod.py", "from ... import nowhere\n")]);
    let graph = tree.extract("pkg/mod.py");
    assert!(graph.targets(EdgeKind::Imports, "pkg/mod.py").is_empty(), "{:#?}", graph.names());
}

#[test]
fn a_star_import_is_a_container_import_plus_the_reexport_shape() {
    let tree = tree(&[
        ("pkg/helpers.py", "def assist():\n    pass\n"),
        ("pkg/mod.py", "from pkg.helpers import *\n"),
    ]);
    let graph = tree.extract("pkg/mod.py");
    assert_eq!(
        graph.targets(EdgeKind::Imports, "pkg/mod.py"),
        vec!["resolved_module pkg.helpers::*".to_string()]
    );
    let reexport = graph.node("pkg.helpers::* as *");
    assert_eq!(reexport.native_kind.as_deref(), Some("reexport"));
    assert_eq!(reexport.name, "*");
    assert_eq!(reexport.container.as_deref(), Some("pkg.mod"), "a re-export is a node of its container");
    assert_eq!(reexport.container_parent, None, "...but never a member of it");
    assert_eq!(
        graph.placeholder_target("pkg.helpers::* as *"),
        ("container:pkg.helpers".into(), "name:*".into(), "pkg.mod".into())
    );
}

/// The acceptance case: a package `__init__` re-exporting through `__all__`,
/// so that `from pkg import Greeter` links through to `pkg/mod.py`.
#[test]
fn dunder_all_in_a_package_init_republishes_what_it_imported() {
    let tree = tree(&[
        ("pkg/mod.py", "class Greeter:\n    pass\n\ndef greet():\n    pass\n"),
        ("pkg/helpers.py", "def assist():\n    pass\n"),
        (
            "pkg/__init__.py",
            "from .mod import Greeter\n\
             from .helpers import assist as helper\n\
             \n\
             def local():\n\
             \x20   pass\n\
             \n\
             __all__ = [\"Greeter\", \"helper\", \"local\", \"never_imported\"]\n",
        ),
    ]);
    let graph = tree.extract("pkg/__init__.py");
    assert_eq!(
        graph.placeholder_target("pkg.mod::Greeter as Greeter"),
        ("container:pkg.mod".into(), "name:Greeter".into(), "pkg".into())
    );
    // A rename is published under the *new* name and addressed at the old one.
    let renamed = graph.node("pkg.helpers::assist as helper");
    assert_eq!(renamed.name, "helper");
    assert_eq!(
        graph.placeholder_target("pkg.helpers::assist as helper"),
        ("container:pkg.helpers".into(), "name:assist".into(), "pkg".into())
    );
    // A name this file declares itself needs no re-export: it is already a
    // member of the container a lookup searches.
    assert!(graph.find("pkg::local as local").is_none(), "{:#?}", graph.names());
    // ...and a name nothing imported has no address to publish.
    let names = graph.names();
    assert!(!names.iter().any(|(qualified, _)| qualified.contains("never_imported")), "{names:#?}");
}

/// `__all__` built at runtime is read as *no* names rather than as a guess -
/// the documented "star-import name sets that depend on runtime" gap.
#[test]
fn a_dynamic_dunder_all_republishes_nothing() {
    let tree = tree(&[
        ("pkg/mod.py", "class Greeter:\n    pass\n"),
        ("pkg/__init__.py", "from .mod import Greeter\n\n__all__ = [n for n in dir()]\n"),
    ]);
    let graph = tree.extract("pkg/__init__.py");
    assert!(
        !graph.names().iter().any(|(_, native)| native.as_deref() == Some("reexport")),
        "{:#?}",
        graph.names()
    );
}

#[test]
fn an_import_of_something_outside_this_project_is_an_external_module() {
    let tree = tree(&[("pkg/mod.py", "import os.path\nfrom pathlib import Path\n")]);
    let graph = tree.extract("pkg/mod.py");
    let node = graph.node("os.path");
    assert_eq!(node.native_kind.as_deref(), Some("external_module"));
    assert_eq!(node.target, None, "core never links an external module, so it carries no target");
    assert_eq!(graph.node("pathlib").native_kind.as_deref(), Some("external_module"));
    // An external import binds a name but addresses nothing of ours: no
    // `pending_symbol` is emitted for `Path`.
    assert!(graph.find("pathlib::Path").is_none(), "{:#?}", graph.names());
}

// --- qualified calls ----------------------------------------------------------

#[test]
fn a_module_qualified_call_is_a_name_key_and_a_class_qualified_one_is_exact() {
    let tree = tree(&[
        ("pkg/helpers.py", "def assist():\n    pass\n"),
        (
            "pkg/mod.py",
            "from . import helpers\n\
             from pkg.base import Base\n\
             \n\
             def through_module():\n\
             \x20   return helpers.assist()\n\
             \n\
             def through_class(obj):\n\
             \x20   return Base.describe(obj)\n",
        ),
    ]);
    let graph = tree.extract("pkg/mod.py");
    // A module qualifier keeps a `name` key, so core's re-export walk can
    // follow a package's `__all__` chain.
    assert_eq!(
        graph.placeholder_target("pkg.helpers::assist"),
        ("container:pkg.helpers".into(), "name:assist".into(), "pkg.mod".into())
    );
    assert_eq!(
        graph.targets(EdgeKind::Calls, "through_module"),
        vec!["pending_symbol pkg.helpers::assist".to_string()]
    );
    // A class qualifier is exact, because a container holds many members
    // merely *named* `describe`.
    assert_eq!(
        graph.placeholder_target("pkg.base::Base.describe"),
        ("container:pkg.base".into(), "qualifiedName:Base.describe".into(), "pkg.mod".into())
    );
}

// --- the receiver gap ---------------------------------------------------------

/// The house rule, asserted: `obj.method()` produces **no edge**, only an
/// open site.
#[test]
fn a_receiver_call_produces_an_open_site_and_no_edge_at_all() {
    let tree = tree(&[(
        "pkg/mod.py",
        "def describe(thing):\n\
         \x20   return thing\n\
         \n\
         def run(obj):\n\
         \x20   return obj.describe()\n",
    )]);
    let graph = tree.extract("pkg/mod.py");
    // There *is* a `describe` in this module; the point is that nothing links
    // `obj.describe()` to it, because `obj`'s type is unknown.
    assert_eq!(graph.edge_between("run", "describe"), None);
    assert!(
        graph.0.edges.iter().all(|edge| edge.kind != EdgeKind::Calls),
        "no call edge may be emitted for a receiver call: {:#?}",
        graph.0.edges
    );
    assert_eq!(graph.open_site_names(), vec![("describe", OpenSiteKind::ReceiverCall)]);
}

/// The one receiver-shaped call this tier does answer, and it does it without
/// trusting the *name* `self` - see `super::bodies`, Decision 7.
#[test]
fn a_call_through_a_methods_first_parameter_resolves_to_that_class() {
    let tree = tree(&[(
        "pkg/mod.py",
        "class Greeter:\n\
         \x20   def shout(s):\n\
         \x20       return s.render()\n\
         \n\
         \x20   def render(s):\n\
         \x20       pass\n\
         \n\
         \x20   @staticmethod\n\
         \x20   def build(s):\n\
         \x20       return s.render()\n\
         \n\
         \x20   def inherited(s):\n\
         \x20       return s.from_a_base()\n",
    )]);
    let graph = tree.extract("pkg/mod.py");
    // The parameter is called `s`, not `self`, and it still resolves: what is
    // read is its *position*, not its name.
    assert_eq!(graph.edge_between("Greeter.shout", "Greeter.render"), Some((EdgeKind::Calls, true)));
    // A `@staticmethod` has no instance parameter, so its first argument is
    // an ordinary value and the call is an open site.
    assert_eq!(graph.edge_between("Greeter.build", "Greeter.render"), None);
    // A member the class does not declare here is not guessed at a base.
    let open: Vec<&str> = graph.open_site_names().into_iter().map(|(name, _)| name).collect();
    assert_eq!(open, vec!["render", "from_a_base"], "{open:?}");
}

// --- class bases --------------------------------------------------------------

#[test]
fn a_class_base_is_a_supertype_edge_same_file_and_imported_alike() {
    let tree = tree(&[(
        "pkg/mod.py",
        "from pkg.base import Base\n\
         \n\
         class Local:\n\
         \x20   pass\n\
         \n\
         class Both(Local, Base, metaclass=Meta):\n\
         \x20   pass\n",
    )]);
    let graph = tree.extract("pkg/mod.py");
    assert_eq!(
        graph.targets(EdgeKind::SupertypeOf, "Both"),
        vec!["Local".to_string(), "pending_symbol pkg.base::Base".to_string()],
        "subtype -> supertype, the direction `find_implementations` walks"
    );
    assert_eq!(graph.edge_between("Both", "Local"), Some((EdgeKind::SupertypeOf, true)));
    // `metaclass=` is configuration, not a base: it produces no
    // `SUPERTYPE_OF` edge even though its value is an ordinary name.
    assert_eq!(graph.targets(EdgeKind::SupertypeOf, "Both").len(), 2);
}

// --- syntax errors and determinism -------------------------------------------

/// A syntax error is a normal answer: whatever parsed is emitted, and the
/// whole file is flagged.
#[test]
fn a_syntax_error_yields_a_partial_graph_that_says_so() {
    let tree = tree(&[(
        "pkg/mod.py",
        "def good():\n\
         \x20   pass\n\
         \n\
         def broken(:\n\
         \x20   pass\n\
         \n\
         def also_good():\n\
         \x20   return good()\n",
    )]);
    let graph = tree.extract("pkg/mod.py");
    assert!(graph.0.nodes.iter().all(|node| node.has_syntax_errors), "{:#?}", graph.names());
    // ...and the declarations on either side of the error are still there,
    // with the edge between them.
    assert_eq!(graph.edge_between("also_good", "good"), Some((EdgeKind::Calls, true)));

    // A file with no error is not flagged, so the flag means something.
    let clean = tree.extract("pkg/base.py");
    assert!(clean.0.nodes.iter().all(|node| !node.has_syntax_errors));
}

#[test]
fn extraction_is_a_pure_function_of_the_source() {
    let tree = tree(&[(
        "pkg/mod.py",
        "from pkg.base import Base\n\nclass C(Base):\n    def m(self):\n        return self.m()\n",
    )]);
    assert_eq!(tree.extract("pkg/mod.py").0, tree.extract("pkg/mod.py").0);
}

/// `id-stability.whitespace-edit`, in miniature: a trailing space before the
/// file's last newline must move nothing.
#[test]
fn a_trailing_space_before_the_last_newline_changes_nothing() {
    let plain = Tree::new(&[("pkg/__init__.py", ""), ("pkg/mod.py", "def f():\n    pass\n")]);
    let spaced = Tree::new(&[("pkg/__init__.py", ""), ("pkg/mod.py", "def f():\n    pass \n")]);
    assert_eq!(plain.extract("pkg/mod.py").0, spaced.extract("pkg/mod.py").0);
}

/// Two branches of a conditional definition are one node, not two lines with
/// one id - see `super::emit`.
#[test]
fn a_conditional_definition_is_indexed_once_under_one_id() {
    let tree = tree(&[(
        "pkg/mod.py",
        "import sys\n\
         \n\
         if sys.version_info >= (3, 11):\n\
         \x20   def load():\n\
         \x20       pass\n\
         else:\n\
         \x20   def load():\n\
         \x20       pass\n",
    )]);
    let graph = tree.extract("pkg/mod.py");
    let loads: Vec<&WireNode> = graph.0.nodes.iter().filter(|node| node.qualified_name == "load").collect();
    assert_eq!(loads.len(), 1, "{:#?}", graph.names());
    // ...and the branch is still walked, which is what makes a conditional
    // import visible at all.
    assert_eq!(graph.targets(EdgeKind::Defines, "pkg/mod.py").iter().filter(|q| *q == "load").count(), 1);
}

/// A conditional *import* is the case that matters: every branch is indexed,
/// none is chosen.
#[test]
fn both_branches_of_a_conditional_import_are_indexed() {
    let tree = tree(&[
        ("pkg/fast.py", "def run():\n    pass\n"),
        ("pkg/slow.py", "def run():\n    pass\n"),
        (
            "pkg/mod.py",
            "try:\n\
             \x20   from pkg.fast import run\n\
             except ImportError:\n\
             \x20   from pkg.slow import run\n",
        ),
    ]);
    let graph = tree.extract("pkg/mod.py");
    assert_eq!(
        graph.targets(EdgeKind::Imports, "pkg/mod.py"),
        vec!["resolved_module pkg.fast::*".to_string(), "resolved_module pkg.slow::*".to_string()]
    );
}

// --- orphans ------------------------------------------------------------------

/// A file no root reaches is still indexed, under its synthetic container -
/// `crate::project`, Decisions 4 and 5.
#[test]
fn an_orphan_files_declarations_are_indexed_under_its_synthetic_container() {
    let tree = Tree::new(&[
        ("pyproject.toml", "[tool.poetry]\npackages = [{ include = \"pkg\", from = \"src\" }]\n"),
        ("src/pkg/__init__.py", ""),
        ("tools/generate.py", "from . import nowhere\n\ndef run():\n    pass\n"),
    ]);
    let graph = tree.extract("tools/generate.py");
    assert_eq!(graph.node("run").container.as_deref(), Some("orphan:tools/generate.py"));
    assert_eq!(graph.node("run").container_parent, None);
    assert!(graph.find("tools.generate").is_none(), "an orphan is a member of nothing");
    // An orphan has no package for a relative import to resolve against.
    assert!(graph.targets(EdgeKind::Imports, "tools/generate.py").is_empty());
}
