//! What the declaration pass learned about one file, and the body pass
//! resolves against: every declaration by the two keys it can be looked up
//! under, every name an `import` bound, and what `__all__` says the file
//! republishes.
//!
//! # Why two passes and a model between them
//!
//! Python executes a module top to bottom, so it is tempting to think one
//! pass would do. It would not, because *use sites* are not executed in that
//! order:
//!
//! ```python
//! def run():
//!     return helper()      # resolved when `run` is called, not when it is defined
//!
//! def helper(): ...
//! ```
//!
//! is ordinary, correct Python, and a single pass reaching `helper()` before
//! `def helper` would have to emit `resolved: false` for it. Within one file
//! that is a *false* claim - `resolved: false` means "core has something left
//! to confirm", and for a target in the same file it has nothing. So
//! [`super::decls`] emits every declaration first and [`super::bodies`]
//! resolves every use site against this model second.
//!
//! # Two lookup keys, for two different questions
//!
//! - **by (scope path, bare name)** answers "what does the bare name
//!   `helper` mean *here*". It is keyed by the lexical scope rather than by
//!   the container, because Python's scopes nest inside one container: a
//!   module, its classes and its functions are all `container = pkg.mod`, and
//!   a container-wide lookup would let a bare `render()` at module level find
//!   `Greeter.render`. The walk out through enclosing scopes is
//!   [`super::scope`]'s job; this map answers one frame at a time.
//! - **by `qualifiedName`** answers "is `Greeter.render` declared here" - the
//!   exact, unambiguous key a class-qualified attribute needs, and the reason
//!   `Cls.m()` is addressed by `qualifiedName` rather than by name. A module
//!   holding `class Reader: def read` and `class Writer: def read` - which is
//!   most modules - offers two declarations *named* `read`, and only the
//!   qualified name tells them apart.
//!
//! An ambiguous `name` lookup is refused rather than guessed, for the reason
//! it is refused everywhere else here: two candidates mean a choice this tier
//! cannot make, and a missing edge beats a wrong one.

use std::collections::HashMap;

use g_mesh_plugin_sdk::wire::{NodeKind, Range};

/// A declaration this file makes, as everything that needs to point at it
/// sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeclRef {
    /// The node's id, which an edge in this same file may name directly.
    pub(crate) id: String,
    /// Its storage kind. Both a filter (core's linker lands a `CALLS` edge
    /// only on a `Function`) and the thing that decides which edge kind a
    /// call site produces: `Greeter()` is a *use* of a class, not a call to a
    /// function, so it is a `REFERENCES` edge. See [`super::bodies`].
    pub(crate) kind: NodeKind,
}

/// What one `import` bound a name to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Import {
    /// The local name is a **module**: `import a.b` (binding `a`),
    /// `import a.b as c` (binding `c`). Members of it are addressed by
    /// `name` key in the container it names.
    Module {
        /// The container key the local name stands for.
        container: String,
    },
    /// The local name is **something inside a module**: `from a.b import C`,
    /// `from . import helpers`. Which of the two it is - a submodule or a
    /// declaration - Python decides at runtime and this tier cannot; see
    /// [`super::bodies`] for how the naming convention chooses between two
    /// addresses without ever deciding whether to emit an edge.
    Item {
        /// The container key the name really lives in.
        container: String,
        /// The name it has there, which an alias does not change.
        name: String,
    },
    /// An import rooted at a dotted name this project does not contain - the
    /// standard library, a third-party distribution. Recorded rather than
    /// forgotten: knowing that `Path` came from `pathlib` is what stops a
    /// later `Path(...)` from being addressed at this project's own
    /// containers.
    External,
}

/// What `__all__` republishes, and where it is written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DunderAll {
    /// The names, in source order, exactly as written.
    pub(crate) names: Vec<String>,
    /// The `__all__` assignment's own range - a re-export node belongs to the
    /// file that is publishing, so its range is the statement that says so.
    pub(crate) range: Option<Range>,
}

/// One file's declarations, imports and `__all__`.
#[derive(Debug, Default)]
pub(crate) struct FileModel {
    by_scope: HashMap<(String, String), Vec<DeclRef>>,
    by_qualified: HashMap<String, DeclRef>,
    imports: HashMap<String, Import>,
    dunder_all: DunderAll,
}

impl FileModel {
    /// Records a declaration made in the frame whose path is `scope`, under
    /// its bare `name` and its full `qualified` path (`Greeter.render`).
    ///
    /// A repeated qualified name - a conditional definition, which
    /// [`Emitter`](super::emit::Emitter) has already merged into one node -
    /// keeps the first, so this table says exactly what the graph says.
    pub(crate) fn declare(&mut self, scope: &str, name: &str, qualified: &str, decl: DeclRef) {
        self.by_qualified.entry(qualified.to_string()).or_insert_with(|| decl.clone());
        let named = self.by_scope.entry((scope.to_string(), name.to_string())).or_default();
        if !named.contains(&decl) {
            named.push(decl);
        }
    }

    /// The one declaration of `name` in the frame whose path is `scope` that
    /// fits `want`, or `None` when there is no such declaration or more than
    /// one.
    pub(crate) fn lookup(&self, scope: &str, name: &str, want: Option<NodeKind>) -> Option<&DeclRef> {
        let candidates = self.by_scope.get(&(scope.to_string(), name.to_string()))?;
        let mut fitting = candidates.iter().filter(|decl| want.is_none_or(|wanted| decl.kind == wanted));
        match (fitting.next(), fitting.next()) {
            (Some(one), None) => Some(one),
            // Several fit, or none of the right kind: a missing edge beats a
            // wrong one.
            _ => None,
        }
    }

    /// The declaration whose full dotted path within this file is `qualified`.
    pub(crate) fn lookup_qualified(&self, qualified: &str) -> Option<&DeclRef> {
        self.by_qualified.get(qualified)
    }

    /// Records what a module-level `import` bound. The first binding of a
    /// name wins, which is also what Python does with the only sane version
    /// of a repeat (two branches of an `if` importing one name from two
    /// places).
    pub(crate) fn import(&mut self, local: &str, import: Import) {
        self.imports.entry(local.to_string()).or_insert(import);
    }

    /// What a module-level `import` bound `local` to.
    pub(crate) fn lookup_import(&self, local: &str) -> Option<&Import> {
        self.imports.get(local)
    }

    /// Records this file's `__all__`. The first one wins, for the same reason
    /// the first import does: a file that writes `__all__` twice has already
    /// made one of them dead.
    pub(crate) fn set_dunder_all(&mut self, names: Vec<String>, range: Range) {
        if self.dunder_all.range.is_none() {
            self.dunder_all = DunderAll { names, range: Some(range) };
        }
    }

    pub(crate) fn dunder_all(&self) -> &DunderAll {
        &self.dunder_all
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use g_mesh_plugin_sdk::wire::Position;

    fn decl(id: &str, kind: NodeKind) -> DeclRef {
        DeclRef { id: id.to_string(), kind }
    }

    fn range() -> Range {
        Range { start: Position { line: 0, col: 0 }, end: Position { line: 0, col: 1 } }
    }

    /// The reason the key is the *scope*, not the container: a module and its
    /// classes share one container key, so a container-wide table would let a
    /// module-level `render()` find `Greeter.render`.
    #[test]
    fn a_method_is_not_visible_under_the_modules_own_scope() {
        let mut model = FileModel::default();
        model.declare("Greeter", "render", "Greeter.render", decl("m", NodeKind::Function));
        assert_eq!(model.lookup("", "render", None), None);
        assert_eq!(model.lookup("Greeter", "render", None).map(|d| d.id.as_str()), Some("m"));
        // ...and it is exact under its qualified name, which is why a
        // class-qualified attribute uses one.
        assert_eq!(model.lookup_qualified("Greeter.render").map(|d| d.id.as_str()), Some("m"));
    }

    #[test]
    fn a_name_two_declarations_share_is_refused_unless_the_kind_singles_one_out() {
        let mut model = FileModel::default();
        model.declare("", "load", "load", decl("f", NodeKind::Function));
        model.declare("", "load", "load", decl("t", NodeKind::Type));
        assert_eq!(model.lookup("", "load", None), None, "no kind to choose by");
        assert_eq!(model.lookup("", "load", Some(NodeKind::Type)).map(|d| d.id.as_str()), Some("t"));
    }

    #[test]
    fn an_external_import_is_known_and_names_no_container_of_ours() {
        let mut model = FileModel::default();
        model.import("Path", Import::External);
        model.import("helpers", Import::Item { container: "pkg".into(), name: "helpers".into() });
        assert_eq!(model.lookup_import("Path"), Some(&Import::External));
        assert!(matches!(model.lookup_import("helpers"), Some(Import::Item { .. })));
        assert_eq!(model.lookup_import("missing"), None);
    }

    #[test]
    fn the_first_dunder_all_wins_and_carries_its_own_range() {
        let mut model = FileModel::default();
        assert_eq!(model.dunder_all().names, Vec::<String>::new());
        model.set_dunder_all(vec!["a".into()], range());
        model.set_dunder_all(vec!["b".into()], range());
        assert_eq!(model.dunder_all().names, vec!["a".to_string()]);
        assert_eq!(model.dunder_all().range, Some(range()));
    }
}
