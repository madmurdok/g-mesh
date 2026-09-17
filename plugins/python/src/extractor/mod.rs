//! The Python structural extractor (GM-296): one file's bytes in, one
//! [`FileGraph`] out.
//!
//! `docs/architecture/multi-language-plugins.md` ("Python plugin") is the
//! design; `crate::project` (GM-295) is the package model this runs on, and
//! the seam between them is [`ProjectContext::container_for`], asked once per
//! file. What follows is the eight decisions this task had to settle rather
//! than infer, each with the module that carries the full argument.
//!
//! # 1. Scope discipline: locals never become placeholders
//!
//! [`scope`] tracks every binding form Python has - parameters, assignments,
//! `for`/`with`/`except` targets, comprehension variables, the walrus
//! operator, function-local imports - on a stack pushed and popped with the
//! syntax. A name that is bound is not a name this file can address
//! elsewhere, so no edge is emitted for it.
//!
//! Two things there are *not* copied from the Rust plugin, because Python's
//! rules differ: a function's locals are bound by a **pre-scan** of its whole
//! body (Python makes a name local to the entire function that assigns it
//! anywhere), and a **class frame is skipped** when resolving a name from
//! inside a method (a class body is not a closure). Both are in [`scope`]'s
//! module doc with the code that motivates them.
//!
//! # 2. `qualifiedName` shapes
//!
//! `f`, `C`, `C.m`, `outer.inner`, `Outer.Inner.m` - the declaration's
//! lexical path within its own module, which is also CPython's own
//! `__qualname__` minus the `<locals>` marker. [`keys`] has the whole
//! argument, including why a method of a *nested* class carries both class
//! names (two classes of one file can each hold an `Inner`, and a node's id
//! has to be injective).
//!
//! # 3. Visibility: everything is `public`
//!
//! Python enforces nothing - there is no `private`, and `from mod import
//! _helper` works. So every declaration is `public` and every one gets an
//! `EXPORTS` edge. [`keys`] argues at length why a leading underscore is
//! **not** modelled as `file` visibility (core would then refuse links for
//! imports that really happen) and why `__all__` is about re-export rather
//! than access.
//!
//! # 4. Imports
//!
//! `import a.b`, `import a.b as c`, `from a.b import name [as alias]`,
//! `from a.b import *`, and every relative form. All five emit an `IMPORTS`
//! edge onto the container they read from; the named forms also emit a
//! `pending_symbol` placeholder, and the star form a `*` re-export. Relative
//! dots resolve against the asking module's own package
//! ([`ModuleCtx::relative`]), which for an `__init__.py` is the package
//! itself. [`decls`] is what each shape emits; [`keys`] is the resolution
//! rule.
//!
//! # 5. `__all__`
//!
//! A `__all__ = [...]` entry naming something this file imported becomes a
//! `reexport` node in this file's own container, so that `from pkg import
//! Greeter` reaches a `class Greeter` declared in `pkg/mod.py` through core's
//! re-export walk. An entry that is not a plain string literal, or that names
//! a declaration of this same file, produces none - see [`decls`].
//!
//! # 6. Conditional definitions and conditional imports
//!
//! Every branch is indexed; no predicate is evaluated. That is the same rule
//! `plugins/rust` applies to `cfg`, and it is what makes `if TYPE_CHECKING:
//! from x import Y` and `try: import fast / except ImportError: import slow`
//! visible at all. Two branches that declare the same name at the same
//! lexical depth are the same id by construction, and [`emit`] merges them
//! into one node (first in source order) instead of emitting a duplicate.
//!
//! # 7. Open sites
//!
//! **Receiver calls, and nothing else.** `obj.method()` produces no edge and
//! one open site; a bare unresolved call (`print`, `len`, a star-imported
//! name) produces neither, because Python's builtins would otherwise be most
//! of the open-site set. `<first parameter>.method()` inside a method *is*
//! resolved, to that class's own member - structurally, by reading the
//! method's first parameter rather than by trusting the name `self`.
//! [`bodies`] has the reasoning and what a future pyright tier inherits.
//!
//! # 8. `.pyi` stubs contribute a `File` node and nothing else
//!
//! `crate::project`'s Decision 6 settles that a stub may contribute no
//! container member and no self-announcement, because its key is the same one
//! its sibling `.py` module computes and two files answering `from pkg import
//! mod` would make core refuse both. This task settles the rest of the
//! question, which that decision left open: a stub contributes **nothing but
//! its own `File` node** - no declarations, no imports, no edges.
//!
//! Emitting a stub's *imports* was considered, since a placeholder is never a
//! container member and so would not breach Decision 6 on its own. It was
//! rejected because a stub's imports exist to type declarations this plugin
//! deliberately does not index: `from collections.abc import Iterator` in a
//! `.pyi` is there so that a signature nobody can see may name `Iterator`.
//! Emitting the dependency without the thing that depends on it describes a
//! file whose content is otherwise invisible, and the stub is still reported
//! (its `File` node), so nothing is silently dropped - which was the only
//! thing Decision 6 required.
//!
//! The file is still **parsed**, because `hasSyntaxErrors` is a fact about
//! the file that core stores whether or not the file declares anything.
//!
//! # What the `File` node is, and is not
//!
//! It is the file's own node, first in the stream, the start of every
//! `DEFINES`/`EXPORTS`/`IMPORTS` edge, and the carrier of the module
//! docstring. It is deliberately **not** a container member:
//! `graph::containers` counts any node with a `container` as one, so a `File`
//! node carrying one would inflate its module's `memberCount` and keep the
//! container alive after its last real declaration went. The node that *is* a
//! member on the module's behalf is the self-announcement node - see
//! [`decls`].

mod bodies;
mod decls;
mod emit;
mod keys;
mod model;
mod scope;
mod syntax;

use std::cell::RefCell;

use g_mesh_plugin_sdk::{Extractor, FileGraph, RelPath};

use crate::extractor::bodies::Bodies;
use crate::extractor::decls::Declarer;
use crate::extractor::emit::Emitter;
use crate::extractor::keys::ModuleCtx;
use crate::extractor::model::FileModel;
use crate::extractor::scope::Scopes;
use crate::extractor::syntax::docstring;
use crate::project::ProjectContext;

/// The plugin's wire identifier: the manifest's `language`, this directory's
/// name, and every node's `language`.
const LANGUAGE: &str = "python";

/// The `engine` label on every edge this tier emits. A free diagnostic
/// string: core branches on the *tier* (`syntactic`), never on this. It is
/// also the label a pyright tier would replace if one is ever built, which is
/// how a reader tells the two apart in an index that holds both.
const ENGINE: &str = "tree-sitter";

thread_local! {
    /// One parser per thread, built once.
    ///
    /// `Parser::set_language` re-checks the grammar's ABI and allocates, and
    /// `--bulk-index` calls `extract` once per file in the project. A
    /// `thread_local` rather than a field of [`PythonExtractor`] because
    /// [`Extractor::extract`] takes `&self` and the trait requires `Sync`: a
    /// `Mutex<Parser>` would serialize a walk that has no reason to be
    /// serialized, and a `RefCell` field would not be `Sync` at all.
    static PARSER: RefCell<tree_sitter::Parser> = RefCell::new({
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .expect("the bundled tree-sitter-python grammar must match the bundled tree-sitter ABI");
        parser
    });
}

/// The [`Extractor`] this binary registers with the SDK's `run` loop.
pub struct PythonExtractor;

impl Extractor for PythonExtractor {
    const LANGUAGE: &'static str = LANGUAGE;
    type Project = ProjectContext;

    fn load_project(&self, root: &std::path::Path) -> anyhow::Result<ProjectContext> {
        ProjectContext::load(root)
    }

    /// One file, in two passes over one parse tree.
    ///
    /// The passes exist because a module's *use sites* do not run in the
    /// order the file is written: `def run(): return helper()` above `def
    /// helper()` is ordinary, correct Python, and a single pass reaching the
    /// call first would have to emit `resolved: false` for it - which within
    /// one file is a false claim, since `resolved: false` means core has
    /// something left to confirm and for a same-file target it has nothing.
    /// So [`Declarer`] emits every declaration first and [`Bodies`] resolves
    /// every use site against it second. See [`model`] for what passes
    /// between them.
    ///
    /// A syntax error is a normal answer: tree-sitter returns a partial tree,
    /// every declaration it did find is emitted, and the graph is marked.
    fn extract(&self, project: &ProjectContext, path: &RelPath, source: &str) -> FileGraph {
        let container = project.container_for(path);
        let module = ModuleCtx::for_file(&container);

        let Some(tree) = PARSER.with(|parser| parser.borrow_mut().parse(source, None)) else {
            // Only reachable through a cancellation flag or a parse timeout,
            // neither of which this plugin sets. The file's own node is still
            // the truth about it.
            return Emitter::new(LANGUAGE, ENGINE, path, source, None).finish();
        };
        let root = tree.root_node();

        let mut emitter = Emitter::new(LANGUAGE, ENGINE, path, source, docstring(root, source));
        // A `.pyi` stub stops here: its `File` node, its syntax-error flag,
        // and nothing else - see this module's doc, Decision 8.
        if module.declares() {
            let mut model = FileModel::default();
            {
                let mut declarer = Declarer {
                    project,
                    module: &module,
                    source,
                    emitter: &mut emitter,
                    model: &mut model,
                    scopes: Scopes::new(),
                };
                declarer.announce(root);
                declarer.collect(root);
                declarer.reexport_dunder_all();
            }
            {
                let file = emitter.file_id().to_string();
                let mut bodies = Bodies::new(&module, source, &model, &mut emitter);
                bodies.visit_children(root, &file);
            }
        }

        if root.has_error() {
            emitter.mark_syntax_errors();
        }
        emitter.finish()
    }
}

#[cfg(test)]
mod tests;
