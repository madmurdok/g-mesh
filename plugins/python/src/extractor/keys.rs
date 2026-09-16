//! Container keys, `qualifiedName`s, visibility and import resolution: the
//! four things every node and every edge this plugin emits has to get right,
//! and the one place the `.` in a Python dotted name is allowed to be split.
//!
//! # Why the plugin splits keys and core does not
//!
//! `core::graph::containers` states flatly that there is "no core rule for
//! splitting keys, because `::`, `.` and `/` mean different things in
//! different languages". That is a rule about *core*. Inside this plugin `.`
//! is Python's own package separator and splitting on it is the language's
//! arithmetic, not a guess: [`parent_key`] is how a relative import's leading
//! dots are resolved, and it is how a self-announcement node finds the
//! `containerParent` of the package it announces itself into. The two
//! statements agree - core learns a parent only from a member's
//! `containerParent` field, which is computed here.
//!
//! # Decision 2: what a `qualifiedName` is for a Python declaration
//!
//! **The declaration's lexical path within its own module**, dot-joined:
//! `f`, `C`, `C.m`, `outer.inner`. The module or package itself is not in the
//! name - that is what the `container` field carries - so `greet` in
//! `pkg/mod.py` is `greet`, not `pkg.mod.greet`. This is the same division of
//! labour the design doc's own table draws (`container` is "the unit a
//! language groups declarations into"; `qualifiedName` is "the name a lookup
//! addresses this symbol by *within* its file or container").
//!
//! ## A method of a nested class is `Outer.Inner.m`
//!
//! The task asks for this one to be argued rather than assumed, and there are
//! two candidates: the full lexical path (`Outer.Inner.m`) or the name
//! relative to the immediately enclosing class (`Inner.m`). **The full
//! lexical path wins, for two independent reasons.**
//!
//! 1. **It is what Python itself calls the thing.** CPython computes
//!    `__qualname__` for that method as exactly `Outer.Inner.m` (PEP 3155),
//!    and it is the expression a caller actually writes to reach it. A
//!    `qualifiedName` a reader has to translate is a `qualifiedName` that
//!    will be mistranslated.
//! 2. **`Inner.m` is not injective, and an id has to be.** A node's id is
//!    `(filePath, kind, qualifiedName, nativeKind)`. One file may hold
//!
//!    ```python
//!    class Request:
//!        class Inner: ...
//!    class Response:
//!        class Inner: ...
//!    ```
//!
//!    and under `Inner.m` both classes' `Inner.m` collapse to one id - the
//!    second silently replacing the first in the index. This is the very
//!    failure `plugins/rust`'s `extractor::keys` records for two inline
//!    modules each declaring `helper`, met again in a different language, and
//!    it is settled the same way: put the whole path in the name. It costs
//!    nothing at lookup time, because a `name`-keyed placeholder matches on
//!    `name`, which stays the bare `m`.
//!
//! The same path rule covers a function nested in a function (`outer.inner`),
//! which is where this plugin *departs* from `__qualname__`: CPython writes
//! `outer.<locals>.inner`. The `<locals>` marker exists in CPython to say
//! "this is a closure's scope, not a class body's", and here that distinction
//! is already carried by `nativeKind` (`function` inside a function, `method`
//! inside a class), so the marker would add a second, uglier spelling of a
//! fact the node already states. The task's own wording (`nested
//! outer.inner`) asks for the shorter form.
//!
//! ## What the path cannot separate, and why that is survivable
//!
//! Two declarations of one file sharing a lexical path *and* a `nativeKind`
//! become one node (`super::emit` merges them, first in source order). The
//! shapes that reach this are exactly the shapes where Python itself binds
//! one name twice: a conditional `def` under `if`/`else`, a redefinition, or
//! a `class outer` beside a `def outer` at one level - which Python resolves
//! by shadowing, so the index saying "one symbol" is not a worse answer than
//! the language's.
//!
//! # Decision 3: visibility - everything is `public`, and the underscore is
//! not visibility
//!
//! **Python enforces nothing.** There is no `private`, no `internal`, no
//! package-private; `from other import _hidden` works, `other._hidden` works,
//! and the interpreter never objects. So every declaration this plugin emits
//! carries [`Visibility::Public`], and every one gets an `EXPORTS` edge
//! alongside its `DEFINES`.
//!
//! ## Why a leading underscore is NOT modelled as `file` visibility
//!
//! It is tempting: `_helper` "means" private, and mapping it to
//! `Visibility::File` would make the index reflect the author's intent. It is
//! also wrong, and the reason is mechanical rather than philosophical. Core's
//! linker (`graph::symbol_links`, contract step 5) reads `file` as "visible
//! iff `fromFile = node.filePath`, and never at all in a file-scoped lookup".
//! So marking `_helper` as `file`-visible would make core **refuse** the link
//! for `from pkg.mod import _helper` - an import that Python executes without
//! complaint, that appears in real code (a package's `__init__` re-exporting
//! its own internals, a test importing the function it tests), and that a
//! user will then ask `find_references` about and be told does not exist.
//!
//! That is not the safe direction of the standing rule. "A missing edge beats
//! a wrong edge" is about edges this tier *cannot prove*; here the edge is
//! proven - the import statement is right there in the source - and a
//! visibility model would be throwing it away on the strength of a naming
//! convention the language does not check. The convention is real and worth
//! respecting, but it is advice to a *human reader*, and the place for it is
//! the name itself, which the index already carries verbatim.
//!
//! ## `__all__` is about re-export, not visibility
//!
//! The other candidate for a visibility signal is `__all__`, and it is a
//! category error for a sharper reason: `__all__` controls only what `from
//! mod import *` binds. A name absent from `__all__` is still importable by
//! name, still reachable as an attribute, and still public in every sense the
//! linker asks about. What `__all__` *does* describe is which names a package
//! republishes on behalf of its submodules - which is a re-export, and is
//! modelled as one (see [`super::decls`]).
//!
//! # Decision 4: resolving an import's module path
//!
//! An absolute `import a.b` / `from a.b import x` names the dotted path
//! outright. A relative `from . import x` / `from ..pkg import y` names it
//! *against the asking module's own package*, and [`ModuleCtx::relative`] is
//! the single rule: one leading dot is the asking module's package, each
//! further dot strips one segment from it, and the written tail is appended.
//!
//! The asking module's package is not the same thing as its container key,
//! and the difference is exactly Decision 2 of `crate::project`: for
//! `pkg/mod.py` the container is `pkg.mod` and the package is `pkg`, but for
//! `pkg/__init__.py` the container *is* `pkg` and so is the package - an
//! `__init__` file is its package, which is why `from . import sibling`
//! written there means `pkg.sibling` and not `pkg.pkg.sibling`. Getting this
//! backwards is silent (every relative import in every package's `__init__`
//! addresses a container that does not exist), which is why
//! [`ModuleCtx::relative`] takes the package from [`ContainerInfo`] rather
//! than deriving it from the key.

use g_mesh_plugin_sdk::wire::Visibility;

use crate::project::ContainerInfo;

/// What this file is, in the project model's terms - see `crate::project`'s
/// Decisions 1, 2, 5 and 6.
///
/// Kept as its own enum rather than carrying the whole [`ContainerInfo`]
/// because the extractor asks only two questions of it (does this file
/// contribute declarations at all, and does it announce itself), and both are
/// answered by the *role*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileRole {
    /// An ordinary `.py` module.
    Module,
    /// A package's own `__init__.py`.
    Package,
    /// A `.pyi` stub: indexed as a file, contributing no declaration and no
    /// self-announcement (`crate::project`, Decision 6).
    Stub,
    /// A file no root reaches (`crate::project`, Decisions 4 and 5).
    Orphan,
}

/// Everything the walk of one file needs to know about *where* that file
/// sits: the container its declarations belong to, that container's own
/// parent, the package its relative imports resolve against, and how it
/// announces itself to that package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModuleCtx {
    /// The container key this file's own top-level declarations belong to -
    /// `pkg.mod` for a module, `pkg` for `pkg/__init__.py`.
    pub(crate) key: String,
    /// That container's own parent key, sent on every member so core can
    /// build the parent chain. `None` for a top-level module or package, and
    /// for an orphan.
    pub(crate) parent: Option<String>,
    /// The package a relative import resolves against - see this module's
    /// doc, Decision 4. `None` when there is no package above this file.
    pub(crate) package: Option<String>,
    /// The bare name this file is addressed by as a member of [`parent`](Self::parent):
    /// `mod` for `pkg/mod.py`, `sub` for `pkg/sub/__init__.py`. `None` for a
    /// stub and an orphan, neither of which announces itself.
    pub(crate) self_name: Option<String>,
    pub(crate) role: FileRole,
}

impl ModuleCtx {
    /// Where the project model places this file.
    pub(crate) fn for_file(info: &ContainerInfo) -> Self {
        match info {
            ContainerInfo::Module { key, parent, name } => Self {
                key: key.clone(),
                parent: parent.clone(),
                package: parent.clone(),
                self_name: Some(name.clone()),
                role: FileRole::Module,
            },
            // An `__init__` file *is* its package: its declarations are the
            // package's members, and `from . import x` written here means a
            // sibling of this file, i.e. a member of this same key.
            ContainerInfo::Package { key, parent, name } => Self {
                key: key.clone(),
                parent: parent.clone(),
                package: Some(key.clone()),
                self_name: Some(name.clone()),
                role: FileRole::Package,
            },
            // A stub keeps its sibling's key (for a future `DECLARATION_OF`)
            // and announces nothing - `crate::project`, Decision 6. Nothing
            // downstream reads `package` for a stub, because a stub's body is
            // never walked.
            ContainerInfo::Stub { key } => Self {
                key: key.clone(),
                parent: parent_key(key).map(str::to_string),
                package: parent_key(key).map(str::to_string),
                self_name: None,
                role: FileRole::Stub,
            },
            // An orphan is its own root: nothing above it is modelled, so it
            // is a member of nothing and a relative import inside it resolves
            // to nothing rather than to something invented.
            ContainerInfo::Orphan { key } => Self {
                key: key.clone(),
                parent: None,
                package: None,
                self_name: None,
                role: FileRole::Orphan,
            },
        }
    }

    /// Whether this file contributes declarations at all. Only a `.pyi` stub
    /// does not - see `crate::project`, Decision 6.
    pub(crate) fn declares(&self) -> bool {
        self.role != FileRole::Stub
    }

    /// The self-announcement node's `(container, containerParent, name)`, or
    /// `None` when this file announces nothing.
    ///
    /// # Why a top-level module announces nothing
    ///
    /// `crate::project`'s Decision 1 requires the announcement so that `from
    /// pkg.sub import mod` finds something *in container `pkg.sub`* named
    /// `mod`. A module or package with no parent - `script.py` or
    /// `pkg/__init__.py` sitting directly in a root - has no such container to
    /// be found in: there is no key for "the root namespace", and core
    /// materializes a container only from its members, so a node with
    /// `container = None` would be a member of nothing and answer no lookup.
    /// Nothing is lost by omitting it, because every way of addressing such a
    /// file goes somewhere else: `import script` is a container import onto
    /// container `script` (which exists as soon as `script.py` declares
    /// anything), and `from script import f` is a `name` key in that same
    /// container.
    pub(crate) fn announcement(&self) -> Option<(String, Option<String>, String)> {
        let name = self.self_name.clone()?;
        let container = self.parent.clone()?;
        let parent = parent_key(&container).map(str::to_string);
        Some((container, parent, name))
    }

    /// The container a relative import names: `level` leading dots and the
    /// written `tail`, resolved against this file's own package - see this
    /// module's doc, Decision 4.
    ///
    /// `None` when the dots reach above the outermost package this project
    /// models (`from ... import x` in a module only two packages deep), which
    /// is also what Python reports as `ImportError: attempted relative import
    /// beyond top-level package` - so answering nothing here is agreeing with
    /// the interpreter, not giving up.
    pub(crate) fn relative(&self, level: usize, tail: &[&str]) -> Option<String> {
        let mut base = self.package.clone()?;
        for _ in 1..level {
            base = parent_key(&base)?.to_string();
        }
        let mut segments: Vec<&str> = base.split('.').collect();
        segments.extend(tail.iter().copied());
        Some(segments.join("."))
    }
}

/// The key of the package that holds `key`, by Python's own path arithmetic.
/// `None` for a top-level name, which is what makes `from .. import x` in a
/// top-level package resolve to nothing rather than to something invented.
pub(crate) fn parent_key(key: &str) -> Option<&str> {
    key.rsplit_once('.').map(|(parent, _)| parent)
}

/// Decision 3: what a Python declaration's visibility is.
///
/// Always [`Visibility::Public`]. The whole argument is in this module's doc;
/// the short version is that Python enforces no access control at all, so
/// anything narrower would be the plugin inventing a rule the language does
/// not have and core would then enforce against real, working imports.
///
/// A function rather than a constant so that every call site reads as a
/// decision being made per declaration - which is what it would become if
/// Python ever grew one - and so that the argument has one place to live.
pub(crate) fn visibility() -> Visibility {
    Visibility::Public
}

/// Whether a declaration with this visibility gets an `EXPORTS` edge - core's
/// `exported` column, which only `public` earns. For Python that is every
/// declaration; the function exists so the rule is stated once rather than
/// assumed at each call site.
pub(crate) fn is_public(visibility: &Visibility) -> bool {
    matches!(visibility, Visibility::Public)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(key: &str, parent: Option<&str>, name: &str) -> ModuleCtx {
        ModuleCtx::for_file(&ContainerInfo::Module {
            key: key.to_string(),
            parent: parent.map(str::to_string),
            name: name.to_string(),
        })
    }

    fn package(key: &str, parent: Option<&str>, name: &str) -> ModuleCtx {
        ModuleCtx::for_file(&ContainerInfo::Package {
            key: key.to_string(),
            parent: parent.map(str::to_string),
            name: name.to_string(),
        })
    }

    #[test]
    fn a_module_announces_itself_to_its_package_and_a_package_to_its_parent() {
        assert_eq!(
            module("pkg.sub.mod", Some("pkg.sub"), "mod").announcement(),
            Some(("pkg.sub".to_string(), Some("pkg".to_string()), "mod".to_string()))
        );
        assert_eq!(
            package("pkg.sub", Some("pkg"), "sub").announcement(),
            Some(("pkg".to_string(), None, "sub".to_string()))
        );
    }

    /// See [`ModuleCtx::announcement`]'s own doc: there is no container above
    /// a root-level file to be a member of.
    #[test]
    fn a_top_level_module_or_package_announces_nothing() {
        assert_eq!(module("script", None, "script").announcement(), None);
        assert_eq!(package("pkg", None, "pkg").announcement(), None);
    }

    #[test]
    fn a_stub_and_an_orphan_declare_or_announce_nothing_of_their_own() {
        let stub = ModuleCtx::for_file(&ContainerInfo::Stub { key: "pkg.mod".into() });
        assert!(!stub.declares());
        assert_eq!(stub.announcement(), None);

        let orphan = ModuleCtx::for_file(&ContainerInfo::Orphan { key: "orphan:tools/gen.py".into() });
        assert!(orphan.declares(), "an orphan's declarations are still indexed, under its own key");
        assert_eq!(orphan.announcement(), None);
        assert_eq!(orphan.relative(1, &["x"]), None, "an orphan has no package to resolve dots against");
    }

    /// Decision 4's whole point: `.` means the *package*, and for an
    /// `__init__` file the package is the file's own container key.
    #[test]
    fn one_dot_is_the_asking_modules_package_and_an_init_file_is_its_own_package() {
        let deep = module("pkg.sub.deep", Some("pkg.sub"), "deep");
        assert_eq!(deep.relative(1, &[]).as_deref(), Some("pkg.sub"));
        assert_eq!(deep.relative(1, &["sibling"]).as_deref(), Some("pkg.sub.sibling"));

        let init = package("pkg.sub", Some("pkg"), "sub");
        assert_eq!(init.relative(1, &[]).as_deref(), Some("pkg.sub"));
        assert_eq!(init.relative(1, &["mod"]).as_deref(), Some("pkg.sub.mod"));
    }

    /// The acceptance case: two dots, from a module two packages deep.
    #[test]
    fn two_dots_strip_one_package_and_the_written_tail_is_appended() {
        let deep = module("pkg.sub.deep", Some("pkg.sub"), "deep");
        assert_eq!(deep.relative(2, &[]).as_deref(), Some("pkg"));
        assert_eq!(deep.relative(2, &["base"]).as_deref(), Some("pkg.base"));
        assert_eq!(deep.relative(2, &["other", "thing"]).as_deref(), Some("pkg.other.thing"));
    }

    /// Python's own `ImportError: attempted relative import beyond top-level
    /// package`, answered the same way: nothing.
    #[test]
    fn dots_that_reach_above_the_top_level_package_resolve_to_nothing() {
        let mod_in_pkg = module("pkg.mod", Some("pkg"), "mod");
        assert_eq!(mod_in_pkg.relative(2, &["x"]), None);
        assert_eq!(mod_in_pkg.relative(3, &["x"]), None);
    }

    #[test]
    fn every_declaration_is_public_and_therefore_exported() {
        assert_eq!(visibility(), Visibility::Public);
        assert!(is_public(&visibility()));
    }
}
