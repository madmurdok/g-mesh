//! One TypeScript or JavaScript file's structural graph, from tree-sitter.
//!
//! The design is `docs/architecture/gm-324-typescript-rust-port.md`
//! (sections 1.2-1.6); declaration lists follow ADR 0024
//! (`docs/adr/0024-semantic-tier-refines-by-binding-a-declaration.md`). In
//! short:
//!
//! - **Grammars.** Three, routed per extension ([`grammar`]): `typescript`
//!   for `.ts/.mts/.cts`, `tsx` for `.tsx`, `javascript` for
//!   `.js/.jsx/.mjs/.cjs`. Every node's `language` is `typescript`.
//! - **Keys.** Node and edge ids are the SDK's
//!   (`g_mesh_plugin_sdk::ids`). Kinds are `File`, `Module`, `Type`,
//!   `Function`, `Variable`; `nativeKind` is part of the id, so a getter and
//!   a setter of one name are two nodes. `qualifiedName` rules are in
//!   [`keys`]. The `File` node's range is the parse root's.
//! - **One id, one node.** A symbol written several times (overloads, a
//!   merged interface or namespace) is one node; with two or more
//!   declarations it carries the list, and its range, signature and doc
//!   comment are settled from it ([`model::FileModel::fill_declaration_lists`]).
//!   Placeholders never record declarations, and the structural tier never
//!   sets `toDeclaration`.
//! - **Draft, then flush.** The walk builds a draft graph ([`model`]) that
//!   can still change a node after creating it; [`emit`] flushes it into the
//!   SDK's builder once, in insertion order, placeholders as plain node specs
//!   with this plugin's own `qualifiedName`.
//! - **Columns are characters**, converted from tree-sitter's bytes by the
//!   SDK's [`CharColumns`]. They differ from UTF-16 columns only after a
//!   non-BMP character on the same line.
//! - **Imports** become placeholders and `IMPORTS` edges ([`imports`]),
//!   resolved through an injected [`imports::SpecifierResolver`], which is
//!   the project model's [`TsProject::resolve`].
//! - **Bodies** are walked with a lexical scope chain, and the calls and
//!   names they use become `CALLS` and `REFERENCES` edges once the walk is
//!   over ([`bodies`]). A local never resolves to this file's symbol of the
//!   same name. Heritage names become `SUPERTYPE_OF` edges.
//! - **Open sites** ([`sites`]) carry what only a type checker can answer:
//!   which overload a call binds, which member of a namespace import a use
//!   names, and what a receiver call reaches. They never reach the wire.
//! - **A syntax error is a normal answer**: whatever the error-tolerant parse
//!   found is emitted, and every node is marked.

pub mod bodies;
pub mod decls;
pub mod emit;
pub mod grammar;
pub mod imports;
pub mod keys;
pub mod model;
pub mod scope;
pub mod sites;
pub mod syntax;

use g_mesh_plugin_sdk::{CharColumns, Extractor, FileGraph, RelPath};

use crate::extractor::decls::Declarer;
use crate::extractor::model::FileModel;
use crate::project::TsProject;

/// The plugin's wire identifier: the manifest's `language`, this directory's
/// name, and every node's `language`.
const LANGUAGE: &str = "typescript";

/// The `engine` label on every edge this tier emits.
const ENGINE: &str = "tree-sitter";

/// The [`Extractor`] this binary registers with the SDK's `run` loop.
pub struct TypeScriptExtractor;

impl Extractor for TypeScriptExtractor {
    const LANGUAGE: &'static str = LANGUAGE;
    type Project = TsProject;

    /// Builds the project model; each config file it skipped is logged.
    fn load_project(&self, root: &std::path::Path) -> anyhow::Result<TsProject> {
        let project = TsProject::load(root)?;
        for note in &project.notes {
            g_mesh_plugin_sdk::log_line!("[{LANGUAGE}] project model: {note}");
        }
        Ok(project)
    }

    /// Keeps the existence set current; touches no disk.
    fn file_presence_changed(&self, project: &mut TsProject, path: &RelPath, present: bool) {
        project.file_presence_changed(path, present);
    }

    /// Parses `source` with its extension's grammar and declares everything
    /// in it. An extension this plugin does not own, or a parse tree-sitter
    /// gives up on, yields the `File` node alone.
    fn extract(&self, project: &TsProject, path: &RelPath, source: &str) -> FileGraph {
        let columns = CharColumns::new(source);
        let tree = grammar::grammar_for(path).and_then(|grammar| grammar::parse(grammar, source));
        let Some(tree) = tree else {
            let model = FileModel::new(path.as_str(), columns.file_range());
            return emit::flush(model, LANGUAGE, ENGINE, path, false);
        };
        let root = tree.root_node();
        let start = root.start_position();
        let end = root.end_position();
        let range = columns.range((start.row, start.column), (end.row, end.column));
        let mut model = FileModel::new(path.as_str(), range);
        let resolve = |specifier: &str, from: &RelPath| project.resolve(specifier, from);
        Declarer::new(source, &columns, path, &resolve, &mut model).run(root);
        emit::flush(model, LANGUAGE, ENGINE, path, root.has_error())
    }
}
