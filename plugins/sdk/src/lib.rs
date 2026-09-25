//! The g-mesh plugin SDK: everything a language plugin needs that is not its
//! language.
//!
//! `docs/architecture/multi-language-plugins.md` ("Interfaces > Plugin SDK")
//! is the design this implements. The short version: a plugin author writes
//! an [`Extractor`] - "given one file's bytes, what nodes and edges are in
//! it" - and, when the language has one, a [`SemanticEngine`]. Everything
//! between that and core is this crate's:
//!
//! - the handshake and the framed control loop ([`run`]);
//! - one-shot `--bulk-index` streaming, nodes before edges, per file;
//! - the project walk (`.gitignore` plus the manifest's `exclude_dirs`);
//! - a per-file cache and the id-keyed incremental diff `fileChanged` answers
//!   with;
//! - id hashing, in the exact scheme the TS and Go plugins use ([`ids`]);
//! - placeholder builders for every target shape core's linker accepts;
//! - the open-site store ([`SdkIndex`]) a semantic tier reads;
//! - starting the semantic engine lazily, on the first `semanticPass` and
//!   never before, with the conformance kit's marker written when it does;
//! - a language-agnostic LSP bridge ([`lsp::LspBridge`]) for the languages
//!   whose semantic tier is a language server;
//! - `workspaceChanged` handling: the project model is rebuilt, nothing else.
//!
//! # The shape of a plugin
//!
//! ```no_run
//! use g_mesh_plugin_sdk::{run, Extractor, FileGraph, PluginSpec, RelPath};
//! use std::path::Path;
//!
//! struct MyExtractor;
//!
//! impl Extractor for MyExtractor {
//!     const LANGUAGE: &'static str = "mylang";
//!     type Project = ();
//!
//!     fn load_project(&self, _root: &Path) -> anyhow::Result<()> {
//!         Ok(())
//!     }
//!
//!     fn extract(&self, _project: &(), path: &RelPath, source: &str) -> FileGraph {
//!         let _ = (path, source);
//!         FileGraph::default()
//!     }
//! }
//!
//! fn main() -> ! {
//!     run(
//!         MyExtractor,
//!         PluginSpec::new("mylang", env!("CARGO_PKG_VERSION"), &[".ml"]),
//!         None,
//!     )
//! }
//! ```
//!
//! # What this crate deliberately does not own
//!
//! Parsing, naming, and every judgement about what an edge means - which is
//! to say, the whole of a language. It also does not own the *contract* it
//! implements: `g-mesh plugins check` does. A plugin built on this SDK is not
//! conformant because it used the SDK; it is conformant because the kit says
//! so, which is why this crate ships [`testing::PluginCheck`] to make running
//! the kit a `#[test]` rather than a habit.
//!
//! # Relationship to the TS plugin
//!
//! `plugins/typescript` has its own Node implementation of everything above
//! and is not being ported onto this crate (the design doc's Open Questions
//! says why). Where a rule here reads as arbitrary, it is almost always
//! copied from that plugin deliberately - the id scheme and the incremental
//! diff especially - because both are cross-plugin contracts and the TS
//! plugin is the implementation the index in the field was built by.

#![deny(missing_docs)]

mod diff;
mod framing;
mod graph;
mod hold;
pub mod ids;
mod index;
pub mod lsp;
mod manifest;
mod path;
mod run;
mod semantic;
pub mod testing;
mod walk;

pub use diff::{diff_file, is_empty_diff};
pub use graph::{
    placeholder_id, render_target, EdgeSpec, FileGraph, FileGraphBuilder, NodeSpec, OpenSite, OpenSiteKind,
    PlaceholderKind,
};
pub use index::{FileEntry, SdkIndex};
pub use manifest::{PluginSpec, ResolvedSpec, MANIFEST_PATH_ENV};
pub use path::RelPath;
pub use run::run;
pub use semantic::{
    write_semantic_engine_marker, SemanticAnswer, SemanticEngine, SemanticEngineFactory, MARKER_DIR_ENV,
    SEMANTIC_ENGINE_MARKER,
};
pub use walk::{walk_project, walk_scope, WalkScope, BASELINE_EXCLUDED_DIRS, MAX_SCOPE_ENTRIES};

/// The wire protocol, re-exported so a plugin needs one dependency rather
/// than two and can never end up compiling a second, different copy of these
/// types than the SDK it hands them to.
pub use g_mesh_wire as wire;

/// An [`Extractor`]'s view of one source file, and the only thing a plugin
/// author has to implement to get a structural plugin.
///
/// # Contract
///
/// - [`extract`](Extractor::extract) is **pure**: it reads `source` and the
///   project model, and nothing else. No filesystem access, no clock, no
///   process id, no mutable global state. That is not tidiness - it is what
///   `id-stability.bulk-repeat` and `id-stability.whitespace-edit` check, and
///   the SDK cannot make an impure extractor pass them.
/// - It must be **deterministic in `source` alone**, given a fixed project
///   model: two walks of an unchanged tree must produce identical ids, and an
///   edit that changes no token must produce an identical graph. An id
///   derived from a line number is fine; one derived from an mtime, a
///   counter, or a hash-map iteration order is not.
/// - A **syntax error is a normal answer**, not a failure: return whatever
///   the error-tolerant parse found, with
///   [`FileGraph::mark_syntax_errors`] set. There is no way to report a
///   parse failure because there is nothing useful core could do with one.
/// - A **panic** is caught per file by the SDK (see [`run`]), so one
///   unparseable file costs that file rather than the project's index. Do not
///   rely on it: a caught panic is a logged defect, not a control-flow
///   mechanism.
pub trait Extractor: Send + Sync {
    /// The plugin's wire identifier, as in the handshake and as every node's
    /// `language`. Must equal the manifest's `language`, which must equal the
    /// plugin directory's name - `g-mesh plugins check` fails the run
    /// otherwise, under `ownership.language` or the handshake itself.
    const LANGUAGE: &'static str;

    /// The extractor's own workspace model: crate roots and a module map for
    /// Rust, a package map for Go, `sys.path` for Python. Rebuilt from
    /// scratch whenever core sends `workspaceChanged`.
    ///
    /// An associated type rather than the design sketch's single
    /// `ProjectContext` struct, because there is nothing for such a struct to
    /// hold that is true of more than one language: a Rust crate graph and a
    /// Go package map share no field. The alternative - a `Box<dyn Any>`
    /// downcast at every `extract` call - would cost a fallible cast per file
    /// to express something the type system can state once. A language with
    /// no workspace model uses `()`.
    type Project: Send + Sync;

    /// Builds the workspace model for `root`, an absolute path to the project
    /// being indexed.
    ///
    /// The one place in a plugin that may read files other than the one being
    /// extracted (`Cargo.toml`, `go.mod`, `pyproject.toml`). An `Err` is
    /// fatal for `--bulk-index` - a walk with no project model would emit a
    /// graph addressed against nothing - and is reported and kept on the
    /// control plane, where the previous model stays in use, since a plugin
    /// that stops answering `fileChanged` takes its language's whole index
    /// stale with it.
    fn load_project(&self, root: &std::path::Path) -> anyhow::Result<Self::Project>;

    /// One file, as a project-relative path and its full text.
    ///
    /// `path` is used verbatim in every id this file produces, so it is the
    /// spelling core uses - forward slashes, relative to the project root -
    /// and the SDK guarantees it ([`RelPath`]).
    fn extract(&self, project: &Self::Project, path: &RelPath, source: &str) -> FileGraph;
}
