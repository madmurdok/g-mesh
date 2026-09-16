//! What the declaration pass learned about one file, and the body pass
//! resolves against: every declaration by the two names it can be looked up
//! under, every name a `use` bound, and every `mod` item.
//!
//! # Why two passes and a model between them
//!
//! A Rust file's items are mutually visible regardless of order - a function
//! at the bottom is callable from the top, and `impl Point` may sit above
//! `struct Point`. A single pass would therefore have to emit an unresolved
//! edge for every forward reference, which within one file is a *false*
//! claim: `resolved: false` means "core has something left to confirm", and
//! for a target in the same file it has nothing. So the declarations are
//! collected first ([`super::decls`]) and the bodies walked second
//! ([`super::bodies`]) against this model.
//!
//! # Two lookup keys, for two different questions
//!
//! - **by name, within a module** answers "what does the bare name `f` mean
//!   here". It refuses an ambiguous answer: two declarations of one name in
//!   one module is either a `cfg` pair this plugin merged already or genuinely
//!   two things, and picking either would be a guess.
//! - **by tail, within a module** answers "is `Point::new` declared here" -
//!   the exact, unambiguous key a type-qualified path needs, and the reason
//!   `T::f()` is addressed by `qualifiedName` rather than by name. A module
//!   holding `impl A { fn new() }` and `impl B { fn new() }` - which is most
//!   modules - offers two declarations *named* `new`, and only the tail tells
//!   them apart.

use std::collections::{HashMap, HashSet};

use g_mesh_plugin_sdk::wire::NodeKind;

use crate::extractor::keys::ModuleNames;

/// A declaration this file makes, as everything that needs to point at it
/// sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeclRef {
    /// The node's id, which an edge in this same file may name directly.
    pub(crate) id: String,
    /// Its storage kind, for the same filter core's linker applies: a
    /// `CALLS` edge only ever lands on a `Function`.
    pub(crate) kind: NodeKind,
}

/// What one `use` item bound a name to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Import {
    /// `use a::b::C [as D]` - `C`, in the container `a::b`.
    Item {
        /// The container key the name really lives in.
        container: String,
        /// The name it has there, which an alias does not change.
        name: String,
    },
    /// A `use` rooted at a crate this project does not model. Recorded rather
    /// than forgotten: knowing that `Serialize` comes from `serde` is what
    /// stops a later `Serialize::…` from being addressed at this project's
    /// own modules.
    External,
}

/// One file's declarations and imports, keyed by the module they are in.
#[derive(Debug, Default)]
pub(crate) struct FileModel {
    by_name: HashMap<(String, String), Vec<DeclRef>>,
    by_tail: HashMap<(String, String), DeclRef>,
    imports: HashMap<(String, String), Import>,
    child_modules: HashSet<(String, String)>,
}

impl FileModel {
    /// Records a declaration in `container`, under its bare `name` and its
    /// full `tail` (`f`, `T::m`, `<T as Tr>::m`).
    ///
    /// A repeated tail - two `cfg` alternatives, which
    /// [`Emitter`](super::emit::Emitter) has already merged into one node -
    /// keeps the first, so this table says exactly what the graph says.
    pub(crate) fn declare(&mut self, container: &str, name: &str, tail: &str, decl: DeclRef) {
        self.by_tail.entry((container.to_string(), tail.to_string())).or_insert_with(|| decl.clone());
        let named = self.by_name.entry((container.to_string(), name.to_string())).or_default();
        if !named.contains(&decl) {
            named.push(decl);
        }
    }

    /// The one declaration of `name` in `container` that fits `want`, or
    /// `None` when there is no such declaration or more than one.
    pub(crate) fn lookup_name(
        &self,
        container: &str,
        name: &str,
        want: Option<NodeKind>,
    ) -> Option<&DeclRef> {
        let candidates = self.by_name.get(&(container.to_string(), name.to_string()))?;
        let mut fitting = candidates.iter().filter(|decl| want.is_none_or(|wanted| decl.kind == wanted));
        match (fitting.next(), fitting.next()) {
            (Some(one), None) => Some(one),
            // Several fit, or none of the right kind: a missing edge beats a
            // wrong one.
            _ => None,
        }
    }

    /// The declaration whose full name within `container` is `tail`.
    pub(crate) fn lookup_tail(&self, container: &str, tail: &str) -> Option<&DeclRef> {
        self.by_tail.get(&(container.to_string(), tail.to_string()))
    }

    /// Records what a `use` item bound. The first binding of a name in a
    /// module wins, which is also what `rustc` does with the only legal
    /// version of a repeat (two `cfg`-gated `use` items of one name).
    pub(crate) fn import(&mut self, container: &str, local: &str, import: Import) {
        self.imports.entry((container.to_string(), local.to_string())).or_insert(import);
    }

    /// What a `use` item of `container` bound `local` to.
    pub(crate) fn lookup_import(&self, container: &str, local: &str) -> Option<&Import> {
        self.imports.get(&(container.to_string(), local.to_string()))
    }

    /// Records that `container` declares `mod name` - file-backed or inline.
    pub(crate) fn child_module(&mut self, container: &str, name: &str) {
        self.child_modules.insert((container.to_string(), name.to_string()));
    }
}

impl ModuleNames for FileModel {
    fn declares_module(&self, module: &str, name: &str) -> bool {
        self.child_modules.contains(&(module.to_string(), name.to_string()))
    }

    fn imported(&self, module: &str, name: &str) -> Option<(String, String)> {
        match self.lookup_import(module, name)? {
            Import::Item { container, name } => Some((container.clone(), name.clone())),
            Import::External => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl(id: &str, kind: NodeKind) -> DeclRef {
        DeclRef { id: id.to_string(), kind }
    }

    #[test]
    fn a_name_two_declarations_share_is_refused_unless_the_kind_singles_one_out() {
        let mut model = FileModel::default();
        model.declare("krate", "new", "A::new", decl("a", NodeKind::Function));
        model.declare("krate", "new", "B::new", decl("b", NodeKind::Function));
        assert_eq!(model.lookup_name("krate", "new", Some(NodeKind::Function)), None);
        assert_eq!(model.lookup_name("krate", "new", None), None);
        // ...but each is exact under its own tail, which is why a
        // type-qualified path uses one.
        assert_eq!(model.lookup_tail("krate", "A::new").map(|d| d.id.as_str()), Some("a"));
        assert_eq!(model.lookup_tail("krate", "B::new").map(|d| d.id.as_str()), Some("b"));
    }

    #[test]
    fn the_kind_filter_separates_a_type_from_a_function_of_one_name() {
        let mut model = FileModel::default();
        model.declare("krate", "Point", "Point", decl("t", NodeKind::Type));
        model.declare("krate", "Point", "Point", decl("f", NodeKind::Function));
        assert_eq!(
            model.lookup_name("krate", "Point", Some(NodeKind::Type)).map(|d| d.id.as_str()),
            Some("t")
        );
        assert_eq!(model.lookup_name("krate", "Point", None), None, "no kind to choose by");
    }

    #[test]
    fn a_module_sees_only_its_own_declarations_and_imports() {
        let mut model = FileModel::default();
        model.declare("krate", "helper", "helper", decl("h", NodeKind::Function));
        model.import("krate", "C", Import::Item { container: "krate::a".into(), name: "C".into() });
        assert!(model.lookup_name("krate::inner", "helper", None).is_none());
        assert!(model.lookup_import("krate::inner", "C").is_none());
        assert!(model.lookup_import("krate", "C").is_some());
    }

    /// An external import is recorded, and deliberately does not answer
    /// `ModuleNames::imported` - nothing in this project can be addressed
    /// through it.
    #[test]
    fn an_external_import_is_known_but_names_no_container() {
        let mut model = FileModel::default();
        model.import("krate", "Serialize", Import::External);
        assert_eq!(model.lookup_import("krate", "Serialize"), Some(&Import::External));
        assert_eq!(model.imported("krate", "Serialize"), None);
    }
}
