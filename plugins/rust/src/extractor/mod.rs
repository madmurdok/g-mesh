//! The Rust structural extractor (GM-286): one file's bytes in, one
//! [`FileGraph`] out.
//!
//! `docs/architecture/multi-language-plugins.md` ("Rust plugin") is the
//! design; `crate::project` (GM-285) is the workspace model this runs on, and
//! the seam between them is [`ProjectContext::container_for`], asked once per
//! file. What follows is the seven decisions this task had to settle rather
//! than infer, each with the module that carries the full argument.
//!
//! # 1. Scope discipline: locals never become placeholders
//!
//! [`scope`] tracks every binding form Rust has - parameters, `let`,
//! closure parameters, `for`/`match`/`if let`/`while let` patterns, generic
//! type and const parameters - on a stack pushed and popped with the syntax.
//! A name that is bound is not a name this file can address elsewhere, so no
//! edge is emitted for it. Where the tracker is unsure it *over*-binds, which
//! costs a missing edge rather than a wrong one - the standing rule
//! (`graph::symbol_links`: "a missing edge beats a wrong one").
//!
//! # 2. `qualifiedName` shapes
//!
//! `f`, `parse::f`, `Lexer::next`, `<Point as Display>::fmt`, `Shape::area` -
//! the item's path from its crate root, without the crate name. [`keys`]
//! has the whole argument, including why a trait impl's method carries the
//! trait (two impls of two traits on one type are otherwise one id) and why
//! the module path is in the name at all (two inline modules in one file are
//! otherwise one id).
//!
//! # 3. Visibility
//!
//! `pub` is `public`; `pub(crate)` is `container(<crate root>)`; `pub(super)`
//! and `pub(in path)` name that module; everything else is
//! `container(<own module>)`. Core reads `container(c)` as "visible to `c`
//! and its descendants", which is exactly Rust's rule for a private item, so
//! the mapping is an identity rather than an approximation - see [`keys`].
//! It holds only if core's parent chain is *complete*, which is why every
//! `mod` item is emitted as a member of the module that declares it; see
//! [`decls`].
//!
//! # 4. `use` resolution
//!
//! `crate::`/`self::`/`super::` resolve against the asking module. A bare
//! first segment is, in order: a crate this workspace models, a `mod` item of
//! the asking module, a module another `use` bound - and otherwise an
//! external crate, which becomes an `external_module` node rather than a
//! guess. [`keys::resolve_module_path`] is the single rule; [`decls`] is what
//! each resolved shape emits.
//!
//! # 5. Macros
//!
//! `macro_rules! m` is a node (a `Function` with `nativeKind = "macro"`, so
//! that `m!()` can be a `CALLS` edge). **Nothing inside a macro body is
//! parsed**, and nothing inside a macro invocation's token tree is either:
//! the grammar hands back tokens rather than expressions there, so a call
//! written inside `assert_eq!(…)` has no call node to find. Items a macro
//! *generates* are therefore invisible. That is the design doc's own
//! documented structural gap, and the plugin README states it plainly rather
//! than half-solving it with a second, ad-hoc parse.
//!
//! # 6. `cfg`
//!
//! Every alternative is indexed; no predicate is evaluated. Two `cfg`
//! branches that declare the same path in one file are the same id by
//! construction, and [`emit`] merges them into one node (first in source
//! order) instead of emitting a duplicate line - see its module doc for the
//! three ways Rust writes one id twice.
//!
//! # 7. Open sites
//!
//! `x.m()`, a call whose path this tier cannot resolve, and an
//! `impl Trait for T` whose `T` is declared in another file. Not unresolved
//! *type* references, which would swamp the bridge with questions about
//! `Vec` and `Option`. [`bodies`] has the reasoning and what GM-290 inherits.
//!
//! # What the `File` node is, and is not
//!
//! It is the file's own node, first in the stream, the start of every
//! `DEFINES` edge, and - new here - the carrier of the file's `//!` header,
//! which documents the module the file backs and has nowhere else to go. It
//! is deliberately **not** a container member: `graph::containers` counts any
//! node with a `container` as one, so a `File` node carrying one would
//! inflate its module's `memberCount` and keep the container alive after its
//! last real declaration went.

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
use crate::extractor::syntax::inner_doc_comment;
use crate::project::ProjectContext;

/// The plugin's wire identifier: the manifest's `language`, this directory's
/// name, and every node's `language`.
const LANGUAGE: &str = "rust";

/// The `engine` label on every edge this tier emits. A free diagnostic
/// string: core branches on the *tier* (`syntactic`), never on this. It is
/// also the label rust-analyzer will replace once GM-290 lands, which is how
/// a reader tells the two apart in an index that holds both.
const ENGINE: &str = "tree-sitter";

thread_local! {
    /// One parser per thread, built once.
    ///
    /// `Parser::set_language` re-checks the grammar's ABI and allocates, and
    /// `--bulk-index` calls `extract` once per file in the project. A
    /// `thread_local` rather than a field of [`RustExtractor`] because
    /// `Extractor::extract` takes `&self` and the trait requires `Sync`: a
    /// `Mutex<Parser>` would serialize a walk that has no reason to be
    /// serialized, and a `RefCell` field would not be `Sync` at all.
    static PARSER: RefCell<tree_sitter::Parser> = RefCell::new({
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("the bundled tree-sitter-rust grammar must match the bundled tree-sitter ABI");
        parser
    });
}

/// The [`Extractor`] this binary registers with the SDK's `run` loop.
pub struct RustExtractor;

impl Extractor for RustExtractor {
    const LANGUAGE: &'static str = LANGUAGE;
    type Project = ProjectContext;

    fn load_project(&self, root: &std::path::Path) -> anyhow::Result<ProjectContext> {
        ProjectContext::load(root)
    }

    /// One file, in two passes over one parse tree.
    ///
    /// The passes exist because a Rust file's items are mutually visible
    /// whatever their order: `impl Point` may sit above `struct Point`, and a
    /// function at the bottom is callable from the top. A single pass would
    /// have to emit `resolved: false` for every forward reference, which
    /// within one file is a false claim - `resolved: false` means core has
    /// something left to confirm, and for a target in the same file it has
    /// nothing. So [`Declarer`] emits every declaration first and [`Bodies`]
    /// resolves every use site against it second. See [`model`] for what
    /// passes between them.
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

        let mut emitter = Emitter::new(LANGUAGE, ENGINE, path, source, inner_doc_comment(root, source));
        let mut model = FileModel::default();
        {
            let mut declarer = Declarer { project, source, emitter: &mut emitter, model: &mut model };
            declarer.collect_modules(root, &module);
            declarer.collect(root, &module, None);
        }
        {
            let file = emitter.file_id().to_string();
            let mut bodies =
                Bodies { project, source, model: &model, emitter: &mut emitter, scopes: Scopes::new() };
            bodies.visit_children(root, &module, None, &file);
        }

        if root.has_error() {
            emitter.mark_syntax_errors();
        }
        emitter.finish()
    }
}

#[cfg(test)]
mod tests;
