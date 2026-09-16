//! Container keys, `qualifiedName`s and visibility: the three things every
//! node this plugin emits has to get right, and the one place the `::` in a
//! Rust path is allowed to be split.
//!
//! # Why the plugin splits keys and core does not
//!
//! `core::graph::containers` states flatly that there is "no core rule for
//! splitting keys, because `::`, `.` and `/` mean different things in
//! different languages". That is a rule about *core*. Inside this plugin
//! `::` is Rust's own path separator and splitting on it is the language's
//! arithmetic, not a guess: [`parent_key`] is how `super` is resolved and how
//! `pub(super)` finds the module it names. The two statements agree - core
//! learns a parent only from a member's `containerParent` field, which is
//! computed here.
//!
//! # Decision 2: what a `qualifiedName` is for a Rust declaration
//!
//! **The item's path from its crate root, without the crate name.** A free
//! function at the top of `lib.rs` is `f`; the same function inside
//! `mod parse` is `parse::f`; an inherent method is `parse::Lexer::next`.
//!
//! The obvious alternative - a name relative to the item's own container, so
//! that `f` is always just `f` - is what the design doc's sketch shows and it
//! does not survive contact with inline modules. A node's id is
//! `(filePath, kind, qualifiedName, nativeKind)`, and
//!
//! ```rust,ignore
//! mod a { pub fn helper() {} }
//! mod b { pub fn helper() {} }
//! ```
//!
//! is one file with two functions that would share all four fields. One of
//! them would silently replace the other in the index. Including the module
//! path is what makes the id injective for every shape Rust can write in one
//! file, and it costs nothing at lookup time: a `name`-keyed placeholder
//! matches on `name`, which stays the bare `helper`.
//!
//! An **orphan** file (one no crate's module tree reaches - see
//! `crate::project`, Decision 5) has a container key of `orphan:<path>` and
//! no crate path at all, so its items are named from its own top level: `f`,
//! `T::m`. Two orphan files are two different `filePath`s, so nothing
//! collides.
//!
//! # Decision 2, continued: `<T as Trait>::m` rather than `T::m`
//!
//! The design doc sketches a trait impl's method as `T::m` distinguished from
//! the inherent `T::m` by `nativeKind = "trait_impl_method"`. That is enough
//! for *one* trait and not for two, which is the ordinary case:
//!
//! ```rust,ignore
//! impl fmt::Display for Point { fn fmt(&self, …) … }
//! impl fmt::Debug   for Point { fn fmt(&self, …) … }
//! ```
//!
//! Both are `Point::fmt`, both are `trait_impl_method`, both are in one file:
//! one id, two declarations, and the second overwrites the first. So the
//! trait goes into the name, in Rust's own syntax for exactly this
//! disambiguation - `<Point as Display>::fmt` and `<Point as Debug>::fmt` -
//! and `nativeKind` stays `trait_impl_method` as the design doc and this
//! task both require. What it costs is that a `Point::fmt()` *path call*
//! (the rare fully-qualified call form; `p.fmt()` is a receiver call and an
//! open site either way) addresses the inherent `Point::fmt` and does not
//! find the trait's. That is a documented structural gap the semantic tier
//! closes, and it is the cheap side of the trade: the alternative loses a
//! declaration outright.

use g_mesh_plugin_sdk::wire::Visibility;
use tree_sitter::Node;

use crate::extractor::syntax::{flatten_path, looks_like_type, Seg};
use crate::project::{ContainerInfo, ProjectContext};

/// The synthetic container prefix `crate::project` gives a file no crate's
/// module tree reaches. Spelled out here (rather than imported) for the same
/// reason the SDK spells core's placeholder kinds out: this module only needs
/// to recognise the shape, and the two are one convention that a test pins
/// (`an_orphan_files_items_are_named_from_the_file_itself`).
const ORPHAN_PREFIX: &str = "orphan:";

/// Which module a walk is currently inside: its container key, that
/// container's own parent, and the crate root both of them hang under.
///
/// One value per *module*, not per file: entering `mod inner { … }` makes a
/// child (see [`ModuleCtx::child`]) and leaving it drops back to this one, so
/// the whole in-file module tree is on the walk's own stack rather than in a
/// map that would have to be kept in step with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModuleCtx {
    /// The container key of this module - `<crate>::<module path>`, or
    /// `orphan:<path>` for an orphan file's own top level.
    pub(crate) key: String,
    /// The key of the module that declares this one. `None` only for a crate
    /// root (and for an orphan file's top level, which is a root of its own).
    pub(crate) parent: Option<String>,
    /// The crate root's key, which `pub(crate)` names.
    pub(crate) crate_root: String,
}

impl ModuleCtx {
    /// The module a file's own top level is, as `ProjectContext` places it.
    pub(crate) fn for_file(info: &ContainerInfo) -> Self {
        match info {
            ContainerInfo::Member { key, parent } => {
                Self { crate_root: crate_root_of(key).to_string(), key: key.clone(), parent: parent.clone() }
            }
            // An orphan file is its own root: nothing above it is modelled,
            // so `pub(crate)` there means "this file and its inline modules",
            // which is the narrowest honest reading.
            ContainerInfo::Orphan { key } => Self { crate_root: key.clone(), key: key.clone(), parent: None },
        }
    }

    /// The module `mod name { … }` (or `mod name;`) declares inside this one.
    pub(crate) fn child(&self, name: &str) -> Self {
        Self {
            key: format!("{}::{}", self.key, name),
            parent: Some(self.key.clone()),
            crate_root: self.crate_root.clone(),
        }
    }

    /// The `qualifiedName` of a member of this module whose own name (within
    /// the module) is `tail` - `f`, `T::m`, `<T as Tr>::m`.
    pub(crate) fn qualified(&self, tail: &str) -> String {
        qualified_in(&self.key, tail)
    }
}

/// See [`ModuleCtx::qualified`] - the same rule for a container key that is
/// not the walk's current one (a placeholder addressed at another module).
pub(crate) fn qualified_in(container: &str, tail: &str) -> String {
    let module_path = module_path_of(container);
    if module_path.is_empty() {
        tail.to_string()
    } else {
        format!("{module_path}::{tail}")
    }
}

/// The crate root key a container key hangs under: its first `::`-separated
/// segment, or the whole key for a crate root or an orphan container.
fn crate_root_of(key: &str) -> &str {
    key.split_once("::").map_or(key, |(root, _)| root)
}

/// A container key's module path within its crate - everything after the
/// crate name, or after an orphan container's file path.
fn module_path_of(key: &str) -> &str {
    let after_root = key.strip_prefix(ORPHAN_PREFIX).unwrap_or(key);
    after_root.split_once("::").map_or("", |(_, path)| path)
}

/// The key of the module that declares `key`, by Rust's own path arithmetic.
/// `None` at a crate root and at an orphan file's top level, which is what
/// makes `super` at a crate root resolve to nothing rather than to something
/// invented.
pub(crate) fn parent_key(key: &str) -> Option<&str> {
    key.rsplit_once("::").map(|(parent, _)| parent)
}

/// What a module-path prefix (`crate::a::b`, `super`, `some_crate::thing`)
/// resolves to, from the point of view of one module.
///
/// The two answers that are not a container are kept apart because they lead
/// to different edges: an [`Unresolved`](PathTarget::Unresolved) prefix
/// produces no edge at all, while an [`ExternalCrate`](PathTarget::ExternalCrate)
/// one produces an `external_module` node and an `IMPORTS` edge, the same
/// shape the TS plugin gives `import "react"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathTarget {
    /// A container key in this project.
    Container(String),
    /// A crate this project does not contain, named by its first segment.
    ExternalCrate(String),
    /// Nothing this plugin can address - see [`flatten_path`]'s own doc.
    Unresolved,
}

/// What the resolver needs to know about the file it is resolving inside,
/// beyond the module it is in: which names that module's own `mod` items and
/// `use` items bound.
///
/// A trait rather than a concrete type so that [`resolve_module_path`] can be
/// unit-tested against a hand-written table - the resolution rules are the
/// part of this plugin most worth testing in isolation, and the real
/// implementor ([`FileModel`](crate::extractor::model::FileModel)) is built
/// by a whole tree walk.
pub(crate) trait ModuleNames {
    /// Whether `module` declares `name` as a `mod` item of its own.
    fn declares_module(&self, module: &str, name: &str) -> bool;
    /// What `module`'s `use` items bound `name` to: the container the name
    /// really lives in, and the name it has there.
    fn imported(&self, module: &str, name: &str) -> Option<(String, String)>;
}

/// Resolves a path prefix to the container it names - "Decision 4" of this
/// task, and the single rule every `use`, every path call and every
/// `impl … for` clause goes through.
///
/// The rules, in the order they are tried, are Rust's own:
///
///  1. `crate::…` starts at the crate root, `self::…` at the asking module,
///     `super::…` one module up per `super`.
///  2. A bare first segment naming a crate this project models is that
///     crate's root. This is the case that makes a workspace's crates see
///     each other, and it comes first because that is what edition 2018+
///     means by a bare path.
///  3. A bare first segment naming a `mod` item of the asking module is that
///     child module. Edition 2015 wrote `use` paths this way, and a *path
///     call* (`helpers::run()`) is written this way in every edition.
///  4. A bare first segment bound by one of the asking module's own `use`
///     items, and spelled like a module rather than a type, continues from
///     wherever that `use` pointed: `use crate::a::b;` then `b::f()`.
///  5. Anything else whose first segment is an identifier is taken to be an
///     external crate; anything else at all is unresolved.
///
/// A prefix that resolves to a container this project does not actually hold
/// is not an error here and never becomes one: core looks the key up among
/// the containers the index really has, finds nothing, and leaves the edge
/// unresolved.
pub(crate) fn resolve_module_path(
    segments: &[Seg<'_>],
    module: &ModuleCtx,
    names: &impl ModuleNames,
    project: &ProjectContext,
) -> PathTarget {
    let Some((first, rest)) = segments.split_first() else { return PathTarget::Unresolved };

    let mut key = match first {
        Seg::Crate => module.crate_root.clone(),
        Seg::SelfMod => module.key.clone(),
        Seg::Super => match parent_key(&module.key) {
            Some(parent) => parent.to_string(),
            None => return PathTarget::Unresolved,
        },
        Seg::Name(name) => {
            if project.crates().iter().any(|krate| krate.key == *name) {
                (*name).to_string()
            } else if names.declares_module(&module.key, name) {
                format!("{}::{}", module.key, name)
            } else if let Some((container, original)) = names.imported(&module.key, name) {
                if looks_like_type(&original) {
                    // `use crate::a::Point;` then `Point::new()` - not a
                    // module path at all. The caller's type-qualified branch
                    // handles it; saying "module" here would address a
                    // container that will never exist.
                    return PathTarget::Unresolved;
                }
                format!("{container}::{original}")
            } else {
                return PathTarget::ExternalCrate((*name).to_string());
            }
        }
    };

    for segment in rest {
        match segment {
            Seg::Name(name) => {
                key.push_str("::");
                key.push_str(name);
            }
            Seg::Super => match parent_key(&key) {
                Some(parent) => key = parent.to_string(),
                None => return PathTarget::Unresolved,
            },
            // `crate`/`self` are only ever path *roots*; in the middle of one
            // they are not Rust, so this is a file with a syntax error whose
            // partial tree reached here.
            Seg::Crate | Seg::SelfMod => return PathTarget::Unresolved,
        }
    }
    PathTarget::Container(key)
}

/// Decision 3: what a `visibility_modifier` means in core's model.
///
/// | Rust | `Visibility` | Why |
/// |---|---|---|
/// | `pub` | `public` | visible from anywhere, including other crates |
/// | `pub(crate)` | `container(<crate root>)` | the crate root is on the parent chain of every module in the crate, and of no module outside it |
/// | `pub(super)` | `container(<parent>)` | |
/// | `pub(in path)` | `container(<that module>)` | |
/// | `pub(self)`, none | `container(<own module>)` | |
///
/// Core reads `container(c)` as "visible to `c` and its descendants"
/// (`graph::symbol_links`, contract step 5), which is exactly Rust's rule for
/// a private item: it is in scope in its own module and in every module
/// nested inside it. So the mapping is an identity, not an approximation -
/// the one thing it depends on is that the parent chain core walks is
/// *complete*, which is why every `mod` item is emitted as a member of the
/// module that declares it (see [`super::decls`]).
///
/// An unresolvable `pub(in …)` falls back to the own module, which is the
/// narrowest reading: it can only refuse a link a correct answer would have
/// allowed.
pub(crate) fn visibility(item: Node, module: &ModuleCtx, source: &str) -> Visibility {
    let Some(modifier) = visibility_modifier(item) else {
        return Visibility::Container(module.key.clone());
    };

    // `pub(in <path>)` is the only form carrying a path, and the grammar
    // marks it with a literal `in` token rather than a field.
    let mut cursor = modifier.walk();
    let children: Vec<Node> = modifier.children(&mut cursor).collect();
    if let Some(index) = children.iter().position(|child| child.kind() == "in") {
        let Some(path) = children.get(index + 1) else { return Visibility::Container(module.key.clone()) };
        return match flatten_path(*path, source)
            .map(|segments| restricted_target(&segments, module))
            .unwrap_or(None)
        {
            Some(key) => Visibility::Container(key),
            None => Visibility::Container(module.key.clone()),
        };
    }

    match children.iter().find_map(|child| match child.kind() {
        "crate" => Some(module.crate_root.clone()),
        "super" => Some(parent_key(&module.key).unwrap_or(&module.crate_root).to_string()),
        "self" => Some(module.key.clone()),
        _ => None,
    }) {
        Some(key) => Visibility::Container(key),
        // A bare `pub` has no parenthesised restriction at all.
        None => Visibility::Public,
    }
}

/// `pub(in <path>)`'s path, resolved against the asking module. Only the
/// three keyword roots are honoured: a restriction is required to name an
/// *ancestor* module, so it can never reach another crate, and resolving a
/// bare first segment as one would be a category error rather than a
/// heuristic.
fn restricted_target(segments: &[Seg<'_>], module: &ModuleCtx) -> Option<String> {
    let (first, rest) = segments.split_first()?;
    let mut key = match first {
        Seg::Crate => module.crate_root.clone(),
        Seg::SelfMod => module.key.clone(),
        Seg::Super => parent_key(&module.key)?.to_string(),
        Seg::Name(_) => return None,
    };
    for segment in rest {
        match segment {
            Seg::Name(name) => {
                key.push_str("::");
                key.push_str(name);
            }
            Seg::Super => key = parent_key(&key)?.to_string(),
            Seg::Crate | Seg::SelfMod => return None,
        }
    }
    Some(key)
}

/// The `visibility_modifier` child of an item, if it has one. Not a field in
/// the grammar, so it is found by kind among the item's own children - never
/// deeper, which is what keeps a `pub` field of a struct from being read as
/// the struct's own.
pub(crate) fn visibility_modifier(item: Node) -> Option<Node> {
    (0..item.child_count())
        .filter_map(|index| item.child(index))
        .find(|child| child.kind() == "visibility_modifier")
}

/// Whether a declaration with this visibility gets an `EXPORTS` edge -
/// core's `exported` column, which only `public` earns.
pub(crate) fn is_public(visibility: &Visibility) -> bool {
    matches!(visibility, Visibility::Public)
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    struct Names {
        modules: HashSet<(String, String)>,
        imports: HashMap<(String, String), (String, String)>,
    }

    impl Names {
        fn empty() -> Self {
            Self { modules: HashSet::new(), imports: HashMap::new() }
        }
        fn with_module(mut self, module: &str, child: &str) -> Self {
            self.modules.insert((module.into(), child.into()));
            self
        }
        fn with_import(mut self, module: &str, local: &str, container: &str, name: &str) -> Self {
            self.imports.insert((module.into(), local.into()), (container.into(), name.into()));
            self
        }
    }

    impl ModuleNames for Names {
        fn declares_module(&self, module: &str, name: &str) -> bool {
            self.modules.contains(&(module.to_string(), name.to_string()))
        }
        fn imported(&self, module: &str, name: &str) -> Option<(String, String)> {
            self.imports.get(&(module.to_string(), name.to_string())).cloned()
        }
    }

    fn module(key: &str, parent: Option<&str>) -> ModuleCtx {
        ModuleCtx {
            crate_root: crate_root_of(key).to_string(),
            key: key.to_string(),
            parent: parent.map(str::to_string),
        }
    }

    fn resolve(segments: &[Seg<'_>], module: &ModuleCtx, names: &Names) -> PathTarget {
        resolve_module_path(segments, module, names, &ProjectContext::default())
    }

    #[test]
    fn the_three_path_keywords_resolve_against_the_asking_module() {
        let here = module("krate::a::b", Some("krate::a"));
        let names = Names::empty();
        assert_eq!(
            resolve(&[Seg::Crate, Seg::Name("x")], &here, &names),
            PathTarget::Container("krate::x".into())
        );
        assert_eq!(
            resolve(&[Seg::SelfMod, Seg::Name("x")], &here, &names),
            PathTarget::Container("krate::a::b::x".into())
        );
        assert_eq!(
            resolve(&[Seg::Super, Seg::Name("x")], &here, &names),
            PathTarget::Container("krate::a::x".into())
        );
        assert_eq!(
            resolve(&[Seg::Super, Seg::Super, Seg::Name("x")], &here, &names),
            PathTarget::Container("krate::x".into())
        );
    }

    /// `super` above a crate root is not Rust, and inventing a parent for it
    /// would put every crate root's private items in one shared scope.
    #[test]
    fn super_above_a_crate_root_resolves_to_nothing() {
        let root = module("krate", None);
        assert_eq!(resolve(&[Seg::Super, Seg::Name("x")], &root, &Names::empty()), PathTarget::Unresolved);
    }

    #[test]
    fn a_bare_first_segment_is_a_child_module_before_it_is_an_external_crate() {
        let here = module("krate::a", Some("krate"));
        let names = Names::empty().with_module("krate::a", "helpers");
        assert_eq!(
            resolve(&[Seg::Name("helpers"), Seg::Name("f")], &here, &names),
            PathTarget::Container("krate::a::helpers::f".into())
        );
        assert_eq!(
            resolve(&[Seg::Name("serde"), Seg::Name("Serialize")], &here, &names),
            PathTarget::ExternalCrate("serde".into())
        );
    }

    #[test]
    fn a_module_imported_by_use_continues_the_path_and_an_imported_type_does_not() {
        let here = module("krate::a", Some("krate"));
        let names = Names::empty().with_import("krate::a", "b", "krate::other", "b").with_import(
            "krate::a",
            "Point",
            "krate::geom",
            "Point",
        );
        assert_eq!(
            resolve(&[Seg::Name("b"), Seg::Name("f")], &here, &names),
            PathTarget::Container("krate::other::b::f".into())
        );
        assert_eq!(
            resolve(&[Seg::Name("Point"), Seg::Name("new")], &here, &names),
            PathTarget::Unresolved,
            "a type-qualified path is not a module path - the caller's own branch handles it"
        );
    }

    #[test]
    fn a_qualified_name_carries_the_module_path_but_never_the_crate_name() {
        assert_eq!(module("krate", None).qualified("f"), "f");
        assert_eq!(module("krate::a::b", Some("krate::a")).qualified("T::m"), "a::b::T::m");
    }

    /// An orphan file has no crate path, so its items are named from its own
    /// top level - and its inline modules still namespace theirs.
    #[test]
    fn an_orphan_files_items_are_named_from_the_file_itself() {
        let orphan = ModuleCtx::for_file(&ContainerInfo::Orphan { key: "orphan:src/dead.rs".into() });
        assert_eq!(orphan.qualified("f"), "f");
        assert_eq!(orphan.child("inner").qualified("f"), "inner::f");
        assert_eq!(orphan.parent, None);
        assert_eq!(orphan.crate_root, "orphan:src/dead.rs");
    }

    #[test]
    fn a_child_modules_parent_is_the_module_that_declares_it() {
        let here = module("krate::a", Some("krate"));
        let child = here.child("inner");
        assert_eq!(child.key, "krate::a::inner");
        assert_eq!(child.parent.as_deref(), Some("krate::a"));
        assert_eq!(child.crate_root, "krate");
    }
}
