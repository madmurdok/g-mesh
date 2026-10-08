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
//!
//! # The later binding of a module-level name
//!
//! Each module-level `def`, `class`, assignment and import statement rebinds
//! its name, and the last one executed wins. This model keeps the start of
//! each binding statement, so [`FileModel::module_binding`] can answer which
//! one a use of the name in this module sees: the latest declaration, an
//! unconditional named import written after it, or a declaration a later
//! `*` import may rebind (which only the linker can decide). It answers per
//! file, not per use site: a use inside a function runs after the module has
//! loaded and so sees the final binding, and module-level code written
//! between two bindings is the one case it gets wrong.
//!
//! A binding made inside a compound statement (`if`/`try`/`with`/...) never
//! displaces an earlier one. `try: from ._speedups import f / except
//! ImportError: from ._pure import f` normally binds the first, and every
//! branch is indexed with no predicate evaluated (see [`super::decls`]).

use std::collections::HashMap;

use g_mesh_plugin_sdk::wire::{NodeKind, Position, Range};

use crate::extractor::syntax::Accessor;

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
    /// The `__all__` assignment's own range, set once the first `__all__` is
    /// read. A re-export node does not take it: it carries the range of the
    /// import that bound its name (see `decls`' `# __all__`).
    pub(crate) range: Option<Range>,
}

/// One file's declarations, imports and `__all__`.
#[derive(Debug, Default)]
pub(crate) struct FileModel {
    by_scope: HashMap<(String, String), Vec<DeclRef>>,
    by_qualified: HashMap<String, DeclRef>,
    /// A property's setter and deleter, keyed by the getter's qualified name.
    /// Kept out of the two tables above on purpose: every name lookup keeps
    /// seeing only the getter, so a bare `x` in the class body or `C.x` does
    /// not turn ambiguous; only a use the body pass knows is a store or a
    /// `del` asks this table.
    accessors: HashMap<(String, Accessor), DeclRef>,
    imports: HashMap<String, Import>,
    /// The range of the import statement that bound each name in `imports`:
    /// where an `__all__` re-export of it is placed, so the re-export node's
    /// position is its statement's order. Kept beside `imports`, not
    /// on [`Import`], whose values are compared as bindings.
    import_ranges: HashMap<String, Range>,
    /// The start of the latest unconditional (directly module-level)
    /// statement that made the binding in `imports`: what decides whether the
    /// import displaced a module-level declaration.
    unconditional_imports: HashMap<String, Position>,
    /// The start of the latest module-level statement declaring each name.
    /// Kept beside `by_scope`, not on [`DeclRef`], which stays a value.
    decl_starts: HashMap<String, Position>,
    /// The start of every non-external `*` import, in source order, branches
    /// included: the linker orders `*` rows by text alone.
    stars: Vec<Position>,
    dunder_all: DunderAll,
}

/// The binding a use of a module-level name sees, when the module declares
/// that name ([`FileModel::module_binding`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModuleBinding<'m> {
    /// An unconditional named import written after the latest declaration
    /// displaced it.
    Import(&'m Import),
    /// The declaration: nothing written after it rebinds the name.
    Decl(&'m DeclRef),
    /// The declaration, with a `*` import written after it that may or may
    /// not provide the name. Only the linker knows what the star provides,
    /// so a use has to be addressed at this module's own container.
    DeclBeforeStar(&'m DeclRef),
}

/// A position as a comparable key: source order.
fn order(position: Position) -> (u32, u32) {
    (position.line, position.col)
}

impl FileModel {
    /// Records a declaration made in the frame whose path is `scope`, under
    /// its bare `name` and its full `qualified` path (`Greeter.render`).
    ///
    /// A repeated qualified name - a conditional definition, which
    /// [`Emitter`](super::emit::Emitter) has already merged into one node -
    /// keeps the first, so this table says exactly what the graph says.
    ///
    /// `start` is the declaring statement's start. For a module-level
    /// declaration (`scope` empty) the latest one is kept, as what a later
    /// import has to follow to displace it ([`FileModel::module_binding`]).
    pub(crate) fn declare(
        &mut self,
        scope: &str,
        name: &str,
        qualified: &str,
        decl: DeclRef,
        start: Position,
    ) {
        if scope.is_empty() {
            let latest = self.decl_starts.entry(name.to_string()).or_insert(start);
            if order(start) > order(*latest) {
                *latest = start;
            }
        }
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

    /// Records a property's setter or deleter under the getter's
    /// `qualified` name. First wins, as in [`FileModel::declare`].
    pub(crate) fn declare_accessor(&mut self, qualified: &str, accessor: Accessor, decl: DeclRef) {
        self.accessors.entry((qualified.to_string(), accessor)).or_insert(decl);
    }

    /// The setter or deleter of the property whose getter's qualified name
    /// is `qualified`, when this file declares one.
    pub(crate) fn accessor(&self, qualified: &str, accessor: Accessor) -> Option<&DeclRef> {
        self.accessors.get(&(qualified.to_string(), accessor))
    }

    /// The declaration whose full dotted path within this file is `qualified`.
    pub(crate) fn lookup_qualified(&self, qualified: &str) -> Option<&DeclRef> {
        self.by_qualified.get(qualified)
    }

    /// Records what a module-level `import` bound.
    ///
    /// `unconditional` says the statement is directly at module level, not
    /// inside a compound statement. A later unconditional binding of a name
    /// replaces an earlier different one, and its range with it, because the
    /// later statement is the one that last bound the name. A conditional one
    /// never replaces a different binding: of two branches importing one
    /// name from two places (`try: ... / except ImportError: ...`) Python
    /// normally runs the first.
    ///
    /// `range` is the import statement's. A repeat of the *same* binding
    /// (`from .a import f` written twice) moves it to the later statement,
    /// conditional or not.
    pub(crate) fn import(&mut self, local: &str, import: Import, range: Range, unconditional: bool) {
        let replaces = match self.imports.get(local) {
            None => true,
            Some(bound) if *bound == import => {
                self.import_ranges.insert(local.to_string(), range);
                if unconditional {
                    self.unconditional_imports.insert(local.to_string(), range.start);
                }
                return;
            }
            Some(_) => unconditional,
        };
        if !replaces {
            return;
        }
        self.imports.insert(local.to_string(), import);
        self.import_ranges.insert(local.to_string(), range);
        if unconditional {
            self.unconditional_imports.insert(local.to_string(), range.start);
        } else {
            self.unconditional_imports.remove(local);
        }
    }

    /// Records a non-external `*` import starting at `start`.
    pub(crate) fn star(&mut self, start: Position) {
        self.stars.push(start);
    }

    /// The binding a use of the module-level `name` sees, when this module
    /// declares one that fits `want`; `None` when it declares none (or more
    /// than one), and only an import can bind the name.
    ///
    /// - [`ModuleBinding::Import`] when an unconditional named import starts
    ///   after the latest module-level declaration.
    /// - [`ModuleBinding::DeclBeforeStar`] when, with no such import, a `*`
    ///   import starts after it.
    /// - [`ModuleBinding::Decl`] otherwise: a declaration after the import
    ///   wins again, and a conditional import never displaces one.
    pub(crate) fn module_binding(&self, name: &str, want: Option<NodeKind>) -> Option<ModuleBinding<'_>> {
        let decl = self.lookup("", name, want)?;
        let Some(declared) = self.decl_starts.get(name).copied().map(order) else {
            return Some(ModuleBinding::Decl(decl));
        };
        if let Some(import) = self.imports.get(name) {
            if self.unconditional_imports.get(name).is_some_and(|start| order(*start) > declared) {
                return Some(ModuleBinding::Import(import));
            }
        }
        if self.stars.iter().any(|start| order(*start) > declared) {
            return Some(ModuleBinding::DeclBeforeStar(decl));
        }
        Some(ModuleBinding::Decl(decl))
    }

    /// Every name a module-level declaration and a later unconditional named
    /// import both bind, where the import displaced the declaration, sorted.
    pub(crate) fn displaced(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .unconditional_imports
            .keys()
            .map(String::as_str)
            .filter(|name| matches!(self.module_binding(name, None), Some(ModuleBinding::Import(_))))
            .collect();
        names.sort_unstable();
        names
    }

    /// The range of the import statement that bound `local`
    /// ([`FileModel::import`]).
    pub(crate) fn import_range(&self, local: &str) -> Option<Range> {
        self.import_ranges.get(local).copied()
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
        model.declare("Greeter", "render", "Greeter.render", decl("m", NodeKind::Function), range().start);
        assert_eq!(model.lookup("", "render", None), None);
        assert_eq!(model.lookup("Greeter", "render", None).map(|d| d.id.as_str()), Some("m"));
        // ...and it is exact under its qualified name, which is why a
        // class-qualified attribute uses one.
        assert_eq!(model.lookup_qualified("Greeter.render").map(|d| d.id.as_str()), Some("m"));
    }

    #[test]
    fn a_name_two_declarations_share_is_refused_unless_the_kind_singles_one_out() {
        let mut model = FileModel::default();
        model.declare("", "load", "load", decl("f", NodeKind::Function), range().start);
        model.declare("", "load", "load", decl("t", NodeKind::Type), range().start);
        assert_eq!(model.lookup("", "load", None), None, "no kind to choose by");
        assert_eq!(model.lookup("", "load", Some(NodeKind::Type)).map(|d| d.id.as_str()), Some("t"));
    }

    #[test]
    fn an_external_import_is_known_and_names_no_container_of_ours() {
        let mut model = FileModel::default();
        model.import("Path", Import::External, range(), true);
        model.import(
            "helpers",
            Import::Item { container: "pkg".into(), name: "helpers".into() },
            range(),
            true,
        );
        assert_eq!(model.lookup_import("Path"), Some(&Import::External));
        assert!(matches!(model.lookup_import("helpers"), Some(Import::Item { .. })));
        assert_eq!(model.lookup_import("missing"), None);
    }

    fn at(line: u32) -> Range {
        Range { start: Position { line, col: 0 }, end: Position { line, col: 1 } }
    }

    fn item(container: &str) -> Import {
        Import::Item { container: container.into(), name: "f".into() }
    }

    /// GM-533 R2: a later unconditional import of a name replaces an earlier
    /// different binding, and its range; a conditional one never does.
    #[test]
    fn a_later_unconditional_import_replaces_and_a_conditional_one_does_not() {
        let mut model = FileModel::default();
        model.import("f", item("a"), at(0), true);
        model.import("f", item("c"), at(1), true);
        assert_eq!(model.lookup_import("f"), Some(&item("c")));
        assert_eq!(model.import_range("f").map(|range| range.start.line), Some(1));
        model.import("f", item("d"), at(2), false);
        assert_eq!(model.lookup_import("f"), Some(&item("c")));
        assert_eq!(model.import_range("f").map(|range| range.start.line), Some(1));
    }

    /// GM-533 R3/R4: the module binding of a declared name, by statement
    /// order.
    #[test]
    fn the_module_binding_is_the_later_statement() {
        let def = decl("d", NodeKind::Function);
        let binding = |imports: &[(u32, bool)], stars: &[u32]| {
            let mut model = FileModel::default();
            model.declare("", "f", "f", def.clone(), at(1).start);
            for &(line, unconditional) in imports {
                model.import("f", item("a"), at(line), unconditional);
            }
            for &line in stars {
                model.star(at(line).start);
            }
            match model.module_binding("f", None) {
                Some(ModuleBinding::Import(_)) => "import",
                Some(ModuleBinding::Decl(_)) => "decl",
                Some(ModuleBinding::DeclBeforeStar(_)) => "decl before star",
                None => "none",
            }
        };
        assert_eq!(binding(&[(2, true)], &[]), "import");
        assert_eq!(binding(&[(0, true)], &[]), "decl");
        assert_eq!(binding(&[(2, false)], &[]), "decl");
        assert_eq!(binding(&[], &[2]), "decl before star");
        assert_eq!(binding(&[], &[0]), "decl");
        assert_eq!(binding(&[(2, true)], &[3]), "import");
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
