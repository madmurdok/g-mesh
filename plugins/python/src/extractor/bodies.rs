//! The body pass: every edge that is not an import, every placeholder that is
//! not a `from … import`, and every open site.
//!
//! # The one question, asked in five shapes
//!
//! Each use site reduces to "what does this name mean, from here":
//!
//! | Written | Addressed as |
//! |---|---|
//! | `helper()`, `CONSTANT` | the nearest enclosing scope's declaration, this module's imports, or nothing |
//! | `mod.helper()` where `mod` is an imported module | a `name` key in the container `mod` resolves to |
//! | `Cls.method()` where `Cls` is an imported class | a `qualifiedName` key - `Cls.method` - in the container `Cls` lives in |
//! | `self.method()` inside a method | that method's own class's member |
//! | `obj.method()` on anything else | **nothing**: an open site for a future semantic tier |
//!
//! The split between the second and third rows is the naming convention
//! ([`looks_like_class`](super::syntax::looks_like_class)), and the reason
//! they are addressed differently is ambiguity. A module holding `class
//! Reader: def read` and `class Writer: def read` - which is most modules -
//! offers two declarations *named* `read`, so a `name`-keyed `Reader.read`
//! would find two candidates and core would rightly refuse both.
//! `qualifiedName` is exact, and `Reader.read`'s qualified name is exactly
//! what this plugin gave it.
//!
//! The reverse trade is why a *module*-qualified call keeps a `name` key:
//! only `name` keys walk re-export chains (`graph::symbol_links`: "a
//! `qualifiedName` names a declaration, never a pass-through"), and
//! `pkg.helpers.assist()` where `pkg.helpers` re-exports `assist` from
//! somewhere else is ordinary Python - it is the whole point of a package's
//! `__all__`.
//!
//! # Decision 7: what becomes an open site, and what does not
//!
//! **Receiver calls, and only receiver calls.** `obj.method()`, where `obj`
//! is a local, a parameter, a call result, or anything else whose type this
//! tier does not know, records one [`OpenSiteKind::ReceiverCall`] site and
//! emits **no edge at all**. That is the house rule stated plainly: the
//! receiver's type is exactly what a structural pass cannot know, and a
//! guessed edge here would be wrong far more often than it was right, because
//! any two classes in a project may have a method of one name.
//!
//! What deliberately does *not* become an open site is a **bare** name that
//! resolves to nothing - `print(x)`, `len(rows)`, `helper()` where `helper`
//! arrived through a star import. `plugins/rust`'s own `bodies` module
//! excludes unresolved *type* references on the grounds that `Vec`, `String`
//! and `Option` "would swamp a bridge that has a per-pass site budget with
//! questions whose answers are not in the index anyway". Python's builtins
//! are the same argument one step further: `print`, `len`, `range`, `open`,
//! `isinstance` and `str` are bare *calls*, they are the most common calls in
//! any Python file, and no semantic engine's answer for them is a node this
//! index holds. Recording them would make the open-site set mostly builtins,
//! which is the same as having no useful open-site set.
//!
//! The star-import case is the one real loss, and it is named as a structural
//! gap in the plugin README rather than half-answered here.
//!
//! **Plus overloaded calls, which refine an edge rather than stand in for
//! one.** A call this tier *did* resolve - `f(1)`, `mod.f(1)`, `self.m(1)` -
//! names the right function and says nothing about which of its `@overload`
//! stubs the call binds; only a type checker knows. Such a call keeps its
//! structural `CALLS` edge and also records an [`OpenSiteKind::OverloadCall`]
//! site whose `replaces` is that edge: always when the target is a node of
//! this file that is an overload set ([`Emitter::overloaded`]), and for every
//! call onto a placeholder, whose target this file cannot see (the semantic
//! engine drops the ones that are not overloaded before asking anything). A
//! receiver call needs nothing extra: its `ReceiverCall` site already goes
//! to the engine, which binds an overload when the answer lands on one. See
//! `docs/adr/0024-semantic-tier-refines-by-binding-a-declaration.md`.
//!
//! # Decision 7, continued: `self.method()` is resolved, and `self` is never
//! trusted
//!
//! The design doc's Rust row resolves `self.m()` inside an `impl` to the impl
//! type's method, and the TS plugin does the same for `this.m()`. Python
//! looks like it cannot have that, because `self` is not a keyword - it is a
//! convention for naming a method's first parameter, and a plain function may
//! use the name without being a method at all.
//!
//! So this plugin does not read the *name*. [`Instance`] records the
//! **first parameter** of the enclosing method, whatever it is called
//! (`self`, `cls`, `s`), together with the class that method is declared in,
//! and `<that parameter>.member(...)` is resolved against that class's own
//! declarations. That is structural: the language itself says the first
//! parameter of a method is the instance it was called on (and, for a
//! `@classmethod`, the class - which has the same members). A
//! `@staticmethod` has no instance parameter and is excluded by name, because
//! that is how Python itself excludes it.
//!
//! Two residual limits, both accepted and both the missing-edge side:
//!
//!  - the class may be subclassed and the member overridden, so the edge
//!    names the statically visible declaration rather than the one that runs.
//!    That is the same residual `plugins/rust` accepts for an inherent method
//!    shadowing a trait method, and it is what `find_callers` wants anyway;
//!  - a member the class does not declare *in this file* (it is inherited)
//!    resolves to nothing and becomes an ordinary receiver open site, rather
//!    than being guessed at the base class.

use g_mesh_plugin_sdk::wire::{EdgeKind, NodeKind, PlaceholderTarget, TargetKey};
use g_mesh_plugin_sdk::{OpenSite, OpenSiteKind, PlaceholderKind};
use tree_sitter::Node;

use crate::extractor::decls::{emit_import_edge, import_edges};
use crate::extractor::emit::{container_target, Emitter};
use crate::extractor::keys::{FileRole, ModuleCtx};
use crate::extractor::model::{ConstValue, DeclRef, FileModel, Import, ImportlibName, ModuleBinding};
use crate::extractor::scope::{dotted_path, FrameKind, Scopes};
use crate::extractor::syntax::{
    decorators, definition_name, dotted_segments, first_parameter_name, has_decorator, inner_definition,
    looks_like_class, plain_string, property_accessor, text, Accessor,
};
use crate::project::ProjectContext;

/// The decorator that says a `def` inside a class body has no instance
/// parameter. Matched on the decorator's own name, so `@staticmethod` counts
/// and `@some.wrapper(staticmethod)` does not.
const STATIC_METHOD: &str = "staticmethod";

/// The enclosing method's instance parameter, and the class it belongs to -
/// see this module's doc, Decision 7.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Instance {
    /// The parameter's name, as written. Never assumed to be `self`.
    parameter: String,
    /// The dotted path of the class that declares the method - `Greeter`, or
    /// `Outer.Inner` for a method of a nested class.
    class_path: String,
}

/// How an attribute use touches the member it names - which of a property's
/// accessors it runs. Only an instance-parameter use (`self.x`) of a property
/// this file declares a setter/deleter for is routed by it; every other use
/// resolves exactly as a load does. See
/// `docs/architecture/gm-511-python-property-accessors.md` section 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    /// A read: `self.x`. The getter.
    Load,
    /// The outermost target of an assignment: `self.x = v`. The setter.
    Store,
    /// A `del` target: `del self.x`. The deleter.
    Delete,
    /// An augmented assignment's target: `self.x += v` reads, then writes.
    /// The getter and the setter.
    Augmented,
}

/// What a use site turned out to name.
#[derive(Debug, Clone)]
enum Bound {
    /// A declaration of this same file: a direct, `resolved: true` edge. The
    /// kind comes along because it decides whether a call site is a `CALLS`
    /// edge or a `REFERENCES` one - `Greeter()` is a *use* of a class.
    Here(String, NodeKind),
    /// Something another file may declare: a placeholder at this address.
    There {
        target: PlaceholderTarget,
        name: String,
        /// Whether the name is spelled like a class, which is the only signal
        /// available for the same `CALLS`-or-`REFERENCES` decision when the
        /// declaration is in another file. Getting it wrong costs an edge
        /// core's kind filter refuses - a missing edge, never a wrong one.
        looks_class: bool,
    },
    /// A receiver whose type this tier does not know - see the module doc,
    /// Decision 7. The caller decides what to do with it, because a *call*
    /// on one is an open site and a plain attribute access is nothing.
    Receiver,
    /// Nothing at all: a local binding, a parameter, a builtin, an external
    /// import. Not a question, so not an open site either.
    Nothing,
}

/// What a dotted name's *prefix* turned out to name - the thing whose member
/// the last segment is.
#[derive(Debug, Clone)]
enum Qualifier {
    /// A module: its members are addressed by `name` key in this container.
    Container(String),
    /// A declaration (a class, almost always): its members are addressed by
    /// `qualifiedName` key within the container the declaration itself lives
    /// in.
    Declaration { container: String, qualified: String },
    /// A runtime value - a local, a parameter, a name nothing here knows. A
    /// call through one is a receiver call, which is the one open site this
    /// plugin records.
    Opaque,
    /// A dotted name known to be **outside** this project - `os`, `pathlib`,
    /// a third-party distribution (`crate::project`'s Decision 8 is what
    /// decides). Kept apart from [`Qualifier::Opaque`] because it produces
    /// neither an edge nor an open site: an open site is a question for a
    /// semantic engine over *this index*, and no engine can answer
    /// `os.path.join` with a node this index holds. Without the distinction,
    /// every standard-library call in the project would become an open site,
    /// which is the flood [`super`]'s Decision 7 exists to avoid.
    External,
}

impl Qualifier {
    /// The qualifier for one segment deeper: `a.b` extended by `c`.
    fn extend(self, segment: &str) -> Self {
        match self {
            Qualifier::Container(container) => Qualifier::Container(format!("{container}.{segment}")),
            Qualifier::Declaration { container, qualified } => {
                Qualifier::Declaration { container, qualified: format!("{qualified}.{segment}") }
            }
            Qualifier::Opaque => Qualifier::Opaque,
            Qualifier::External => Qualifier::External,
        }
    }
}

/// Which dynamic-import function a call's callee statically is (GM-510).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DynamicCallee {
    /// `importlib.import_module`, however `importlib` or it was bound.
    ImportModule,
    /// The builtin `__import__`, unshadowed.
    DunderImport,
}

/// The segments of a dotted module name, when every one is an identifier;
/// `None` for anything a real import could not name (`pkg..mod`, `a b`).
fn module_segments(name: &str) -> Option<Vec<&str>> {
    let segments: Vec<&str> = name.split('.').collect();
    let valid = |segment: &&str| {
        segment.chars().next().is_some_and(|first| first == '_' || first.is_alphabetic())
            && segment.chars().all(|ch| ch == '_' || ch.is_alphanumeric())
    };
    segments.iter().all(valid).then_some(segments)
}

/// CPython's `importlib._bootstrap._resolve_name`: `level` leading dots and
/// the `tail` after them, against `package`. `None` where CPython raises
/// (`attempted relative import beyond top-level package`): `package` has
/// fewer than `level` segments.
fn resolve_name(tail: &[&str], package: &str, level: usize) -> Option<String> {
    let bits: Vec<&str> = package.rsplitn(level, '.').collect();
    if bits.len() < level {
        return None;
    }
    let base = bits.last()?;
    let mut segments = vec![*base];
    segments.extend(tail.iter().copied());
    Some(segments.join("."))
}

/// Walks a file's bodies against what [`Declarer`](super::decls::Declarer)
/// found.
///
/// Every question this pass asks about a *name* is about what the file
/// itself established - its own declarations, and what its own import
/// statements bound. The [`ProjectContext`] it holds is consulted for one
/// thing only: whether an import this pass emits (a function-local or
/// class-body one, or a dynamic one - GM-510) names a container of this
/// project, the same question `super::decls` asks of a module-level import,
/// through the same [`emit_import_edge`].
pub(crate) struct Bodies<'a, 's> {
    pub(crate) project: &'a ProjectContext,
    pub(crate) module: &'a ModuleCtx,
    pub(crate) source: &'s str,
    pub(crate) model: &'a FileModel,
    pub(crate) emitter: &'a mut Emitter<'s>,
    pub(crate) scopes: Scopes,
    instance: Option<Instance>,
}

impl<'a, 's> Bodies<'a, 's> {
    pub(crate) fn new(
        project: &'a ProjectContext,
        module: &'a ModuleCtx,
        source: &'s str,
        model: &'a FileModel,
        emitter: &'a mut Emitter<'s>,
    ) -> Self {
        Self { project, module, source, model, emitter, scopes: Scopes::new(), instance: None }
    }

    pub(crate) fn visit_children(&mut self, node: Node, from: &str) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.visit(child, from);
        }
    }

    /// Walks one node. `from` is the declaration an edge found here starts
    /// at: the enclosing `def`/`class`, or the file's own `File` node at
    /// module level.
    fn visit(&mut self, node: Node, from: &str) {
        match node.kind() {
            "decorated_definition" => {
                let inner = inner_definition(node);
                match inner.kind() {
                    "function_definition" => self.function(node, inner, from),
                    "class_definition" => self.class(node, inner, from),
                    _ => {}
                }
            }
            "function_definition" => self.function(node, node, from),
            "class_definition" => self.class(node, node, from),
            // At module level pass 1 read these; walking them again would
            // emit a second, duplicate reference for every imported name.
            // Below a `def` or `class` pass 1 does not look, so the
            // statement's `IMPORTS` edges are drawn here, from the file node
            // (GM-510, D7a). What it binds stays a plain local
            // (`super::scope`), so nothing else is emitted.
            "import_statement" | "import_from_statement" => {
                if self.scopes.kind() != FrameKind::Module {
                    let range = self.emitter.positions().range(node);
                    for edge in import_edges(node, self.source, self.project, self.module) {
                        emit_import_edge(
                            self.project,
                            self.emitter,
                            &edge.container,
                            &edge.specifier,
                            range,
                            edge.relative,
                        );
                    }
                }
            }
            // `global x` / `nonlocal x` name a binding, not a use of one.
            "global_statement" | "nonlocal_statement" => {}
            "comment" => {}
            "call" => self.call(node, from),
            "attribute" => self.attribute(node, from, false, Access::Load),
            "identifier" => self.bare(node, from, false),
            "delete_statement" => {
                let mut cursor = node.walk();
                let targets: Vec<Node> = node.named_children(&mut cursor).collect();
                for target in targets {
                    self.delete_target(target, from);
                }
            }
            "assignment" | "augmented_assignment" => self.assignment(node, from),
            "named_expression" => {
                if let Some(value) = node.child_by_field_name("value") {
                    self.visit(value, from);
                }
                if let Some(name) = node.child_by_field_name("name") {
                    self.scopes.bind_target(name, self.source);
                }
            }
            "lambda" => self.lambda(node, from),
            "for_statement" | "for_in_clause" => self.for_like(node, from),
            "as_pattern" => self.as_pattern(node, from),
            // `f(retries=3)`: `retries` is a parameter name, not a symbol this
            // index carries. Only the value is a use.
            "keyword_argument" => {
                if let Some(value) = node.child_by_field_name("value") {
                    self.visit(value, from);
                }
            }
            _ => self.visit_children(node, from),
        }
    }

    // --- definitions ----------------------------------------------------------

    fn function(&mut self, outer: Node, inner: Node, from: &str) {
        let Some(name) = definition_name(inner, self.source).map(str::to_string) else { return };
        let qualified = self.scopes.child_path(&name);
        // A property's setter/deleter is its own node, found in the accessor
        // table rather than under the qualified name it shares with the
        // getter - the same recognition `super::decls` declared it by.
        let accessor = if self.scopes.kind() == FrameKind::Class {
            property_accessor(outer, &name, self.source)
        } else {
            None
        };
        let own = match accessor {
            Some(accessor) => self.model.accessor(&qualified, accessor),
            None => self.model.lookup_qualified(&qualified),
        };
        let id = own.map(|decl| decl.id.clone()).unwrap_or_else(|| from.to_string());

        // Decorators and the signature's own expressions - annotations and
        // default values - are evaluated in the *enclosing* scope, before
        // this function's parameters exist. Walking them here rather than
        // inside the frame is what keeps `def f(x: Config = DEFAULT)` a use of
        // `Config` and `DEFAULT` rather than of two locals.
        self.visit_decorators(outer, &id);
        if let Some(parameters) = inner.child_by_field_name("parameters") {
            self.visit_parameter_expressions(parameters, &id);
        }
        if let Some(return_type) = inner.child_by_field_name("return_type") {
            #[cfg(test)]
            crate::census::push_ctx(crate::census::Ctx::Annotation);
            self.visit(return_type, &id);
            #[cfg(test)]
            crate::census::pop_ctx();
        }

        // The instance parameter, for `self.member(...)` - see Decision 7.
        // A nested `def` inside a method *inherits* it, because a closure
        // genuinely does see the enclosing method's `self`.
        let instance =
            if self.scopes.kind() == FrameKind::Class && !has_decorator(outer, self.source, STATIC_METHOD) {
                first_parameter_name(inner, self.source).map(|parameter| Instance {
                    parameter: parameter.to_string(),
                    class_path: self.scopes.path().to_string(),
                })
            } else {
                self.instance.clone()
            };
        let saved = std::mem::replace(&mut self.instance, instance);

        self.scopes.push(FrameKind::Function, qualified);
        if let Some(parameters) = inner.child_by_field_name("parameters") {
            self.scopes.bind_parameters(parameters, self.source);
        }
        if let Some(body) = inner.child_by_field_name("body") {
            self.scopes.bind_function_locals(body, self.source);
            self.visit_children(body, &id);
        }
        self.scopes.pop();
        self.instance = saved;
    }

    fn class(&mut self, outer: Node, inner: Node, from: &str) {
        let Some(name) = definition_name(inner, self.source).map(str::to_string) else { return };
        let qualified = self.scopes.child_path(&name);
        let id = self
            .model
            .lookup_qualified(&qualified)
            .map(|decl| decl.id.clone())
            .unwrap_or_else(|| from.to_string());

        self.visit_decorators(outer, &id);
        if let Some(bases) = inner.child_by_field_name("superclasses") {
            self.superclasses(bases, &id);
        }

        self.scopes.push(FrameKind::Class, qualified);
        if let Some(body) = inner.child_by_field_name("body") {
            self.visit_children(body, &id);
        }
        self.scopes.pop();
    }

    /// `class Sub(Base, mixins.Loud, metaclass=Meta)` - a `SUPERTYPE_OF` edge
    /// from the class to each positional base, in the direction
    /// `find_implementations` walks (subtype -> supertype).
    ///
    /// A keyword argument is not a base: `metaclass=`, and PEP 487's
    /// `__init_subclass__` keywords, are configuration. Its *value* is still
    /// an ordinary reference, so it is walked as one.
    ///
    /// A base this tier cannot address - `object`, a base built by a call
    /// (`class C(namedtuple("C", "x"))`), a generic alias it cannot flatten -
    /// produces no edge and **no open site**. See Decision 7: the open-site
    /// budget is spent on receiver calls, and `object` would be the most
    /// common entry in it by a wide margin.
    fn superclasses(&mut self, bases: Node, subtype: &str) {
        let mut cursor = bases.walk();
        for argument in bases.named_children(&mut cursor) {
            if argument.kind() == "keyword_argument" {
                if let Some(value) = argument.child_by_field_name("value") {
                    self.visit(value, subtype);
                }
                continue;
            }
            // `class C(Protocol[T])` - the base is the subscripted name, and
            // the type arguments are ordinary references.
            let base = if argument.kind() == "subscript" {
                if let Some(index) = argument.child_by_field_name("subscript") {
                    self.visit(index, subtype);
                }
                match argument.child_by_field_name("value") {
                    Some(value) => value,
                    None => continue,
                }
            } else {
                argument
            };
            #[cfg(test)]
            crate::census::push_ctx(crate::census::Ctx::Base);
            let bound = match base.kind() {
                "identifier" => self.resolve_bare(text(base, self.source), Some(NodeKind::Type)),
                "attribute" => match dotted_segments(base, self.source) {
                    Some(segments) => self.resolve_path(&segments),
                    None => Bound::Nothing,
                },
                _ => {
                    #[cfg(test)]
                    crate::census::pop_ctx();
                    self.visit(base, subtype);
                    continue;
                }
            };
            #[cfg(test)]
            crate::census::pop_ctx();
            match bound {
                Bound::Here(to, _) => {
                    self.emitter.resolved_edge(EdgeKind::SupertypeOf, subtype, &to);
                }
                Bound::There { target, name, .. } => {
                    let range = self.emitter.positions().range(base);
                    let placeholder =
                        self.emitter.placeholder(PlaceholderKind::PendingSymbol, &name, target, range);
                    self.emitter.placeholder_edge(EdgeKind::SupertypeOf, subtype, &placeholder);
                }
                Bound::Receiver | Bound::Nothing => {}
            }
        }
    }

    fn visit_decorators(&mut self, outer: Node, from: &str) {
        for decorator in decorators(outer) {
            let mut cursor = decorator.walk();
            let children: Vec<Node> = decorator.named_children(&mut cursor).collect();
            #[cfg(test)]
            crate::census::push_ctx(crate::census::Ctx::Decorator);
            for child in children {
                self.visit(child, from);
            }
            #[cfg(test)]
            crate::census::pop_ctx();
        }
    }

    /// A parameter list's annotations and default values - the only parts of
    /// it that are expressions rather than bindings.
    fn visit_parameter_expressions(&mut self, parameters: Node, from: &str) {
        let mut cursor = parameters.walk();
        let children: Vec<Node> = parameters.named_children(&mut cursor).collect();
        for parameter in children {
            for field in ["type", "value"] {
                if let Some(child) = parameter.child_by_field_name(field) {
                    #[cfg(test)]
                    if field == "type" {
                        crate::census::push_ctx(crate::census::Ctx::Annotation);
                    }
                    self.visit(child, from);
                    #[cfg(test)]
                    if field == "type" {
                        crate::census::pop_ctx();
                    }
                }
            }
        }
    }

    // --- statements and expressions -------------------------------------------

    /// A name being assigned is not a use of it; an attribute or subscript
    /// target still uses its own object (`self.count = 1` reads `self`,
    /// `table[key] = v` reads `table` and `key`).
    fn assignment(&mut self, node: Node, from: &str) {
        for field in ["type", "right", "value"] {
            if let Some(child) = node.child_by_field_name(field) {
                #[cfg(test)]
                if field == "type" {
                    crate::census::push_ctx(crate::census::Ctx::Annotation);
                }
                self.visit(child, from);
                #[cfg(test)]
                if field == "type" {
                    crate::census::pop_ctx();
                }
            }
        }
        // An annotation with no value (`self.x: int`) stores nothing.
        let access = if node.kind() == "augmented_assignment" {
            Access::Augmented
        } else if node.child_by_field_name("right").is_some() {
            Access::Store
        } else {
            Access::Load
        };
        if let Some(left) = node.child_by_field_name("left") {
            self.visit_target(left, from, access);
        }
    }

    /// One assignment target. `access` applies to an attribute that *is* the
    /// target; its object (`self.x` in `self.x.y = v`) is an ordinary load.
    fn visit_target(&mut self, target: Node, from: &str, access: Access) {
        match target.kind() {
            "identifier" => {}
            "attribute" => self.attribute(target, from, false, access),
            "subscript" => self.visit(target, from),
            _ => {
                let mut cursor = target.walk();
                let children: Vec<Node> = target.named_children(&mut cursor).collect();
                for child in children {
                    self.visit_target(child, from, access);
                }
            }
        }
    }

    /// One `del` target. `del a.x, b.y` is a `delete_statement` over an
    /// `expression_list`; a parenthesised or bracketed target list nests the
    /// same way. Anything but an attribute is walked as it always was.
    fn delete_target(&mut self, target: Node, from: &str) {
        match target.kind() {
            "attribute" => self.attribute(target, from, false, Access::Delete),
            "expression_list" | "tuple" | "list" | "parenthesized_expression" => {
                let mut cursor = target.walk();
                let children: Vec<Node> = target.named_children(&mut cursor).collect();
                for child in children {
                    self.delete_target(child, from);
                }
            }
            _ => self.visit(target, from),
        }
    }

    /// A lambda is a scope of its own, sharing the enclosing frame's dotted
    /// path because it declares nothing that could be named.
    fn lambda(&mut self, node: Node, from: &str) {
        if let Some(parameters) = node.child_by_field_name("parameters") {
            self.visit_parameter_expressions(parameters, from);
        }
        let path = self.scopes.path().to_string();
        self.scopes.push(FrameKind::Function, path);
        if let Some(parameters) = node.child_by_field_name("parameters") {
            self.scopes.bind_parameters(parameters, self.source);
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.visit(body, from);
        }
        self.scopes.pop();
    }

    /// `for x in items:` and a comprehension's `for x in items`: the iterable
    /// is evaluated before the target is bound, which is what keeps
    /// `for row in row` a read of the outer `row`.
    fn for_like(&mut self, node: Node, from: &str) {
        let left = node.child_by_field_name("left");
        let right = node.child_by_field_name("right");
        if let Some(right) = right {
            self.visit(right, from);
        }
        if let Some(left) = left {
            self.scopes.bind_target(left, self.source);
        }
        let mut cursor = node.walk();
        let children: Vec<Node> = node.named_children(&mut cursor).collect();
        for child in children {
            if Some(child) == left || Some(child) == right {
                continue;
            }
            self.visit(child, from);
        }
    }

    /// `with open(p) as fh:` and `except ValueError as err:` - the value is a
    /// use, the alias is a binding.
    fn as_pattern(&mut self, node: Node, from: &str) {
        let alias = node.child_by_field_name("alias");
        let mut cursor = node.walk();
        let children: Vec<Node> = node.named_children(&mut cursor).collect();
        for child in children {
            if Some(child) == alias {
                continue;
            }
            self.visit(child, from);
        }
        if let Some(alias) = alias {
            self.scopes.bind_target(alias, self.source);
        }
    }

    fn call(&mut self, node: Node, from: &str) {
        let Some(function) = node.child_by_field_name("function") else {
            self.visit_children(node, from);
            return;
        };
        match function.kind() {
            "identifier" => self.bare(function, from, true),
            "attribute" => self.attribute(function, from, true, Access::Load),
            // Calling something that is not a name at all - `(f)()`,
            // `handlers[key]()`, `factory()()`. The callee is an expression;
            // walking it is the honest answer.
            _ => self.visit(function, from),
        }
        if let Some(arguments) = node.child_by_field_name("arguments") {
            // A call's arguments are ordinary expression position, whatever
            // position the call itself sits in - `@route("/x", Foo)` is a
            // decorator, `Foo` inside it is not a decorator name.
            #[cfg(test)]
            crate::census::push_ctx(crate::census::Ctx::Other);
            self.visit(arguments, from);
            #[cfg(test)]
            crate::census::pop_ctx();
        }
        self.dynamic_import(node);
    }

    // --- dynamic imports (GM-510) ---------------------------------------------

    /// `importlib.import_module(name, package)` or `__import__(name, ...)`
    /// whose module name folds to a string from this file alone: the same
    /// `IMPORTS` edge the static `import <name>` draws, from the file node,
    /// with the module-name argument's range. Anything that does not fold
    /// draws nothing - a missing edge, never a wrong one.
    fn dynamic_import(&mut self, call: Node) {
        let Some(function) = call.child_by_field_name("function") else { return };
        let Some(callee) = self.dynamic_import_callee(function) else { return };
        let Some(arguments) = call.child_by_field_name("arguments") else { return };
        if arguments.kind() != "argument_list" {
            return;
        }
        let mut positional = Vec::new();
        let mut keywords = Vec::new();
        let mut cursor = arguments.walk();
        for argument in arguments.named_children(&mut cursor) {
            match argument.kind() {
                "keyword_argument" => {
                    let name = argument.child_by_field_name("name");
                    let value = argument.child_by_field_name("value");
                    if let (Some(name), Some(value)) = (name, value) {
                        keywords.push((text(name, self.source), value));
                    }
                }
                // `f(*args)` / `f(**kwargs)`: which argument is which is a
                // runtime question.
                "list_splat" | "dictionary_splat" => return,
                "comment" => {}
                _ => positional.push(argument),
            }
        }
        let argument = |index: usize, keyword: &str| {
            positional
                .get(index)
                .copied()
                .or_else(|| keywords.iter().find(|(name, _)| *name == keyword).map(|(_, value)| *value))
        };
        let Some(name_node) = argument(0, "name") else { return };
        let Some(name) = self.fold(name_node) else { return };
        let range = self.emitter.positions().range(name_node);
        let level = name.chars().take_while(|ch| *ch == '.').count();
        if callee == DynamicCallee::DunderImport {
            // D4: Python 3's default `level` is 0 (absolute); a relative
            // `__import__` is not folded. The edge goes onto the whole dotted
            // name, as the static `import a.b` draws it.
            if let Some(level) = argument(4, "level") {
                if level.kind() != "integer" || text(level, self.source) != "0" {
                    return;
                }
            }
            if level > 0 {
                return;
            }
        }
        if level == 0 {
            if module_segments(&name).is_some() {
                emit_import_edge(self.project, self.emitter, &name, &name, range, false);
            }
            return;
        }
        // D5: a leading-dot name resolves against `package`, CPython's own
        // `_resolve_name`.
        let tail_text = &name[level..];
        let tail = if tail_text.is_empty() {
            Vec::new()
        } else {
            match module_segments(tail_text) {
                Some(tail) => tail,
                None => return,
            }
        };
        let Some(package_node) = argument(1, "package") else { return };
        let Some((package, own)) = self.fold_package(package_node) else { return };
        if module_segments(&package).is_none() {
            return;
        }
        let Some(resolved) = resolve_name(&tail, &package, level) else { return };
        if own {
            // `__package__` / `__name__`: this file's own package, so the
            // static relative rule applies - always one of ours, the name as
            // written for the specifier (D5a).
            emit_import_edge(self.project, self.emitter, &resolved, &name, range, true);
        } else {
            // A literal or constant package: an absolute import of the
            // resolved name, which `has_container` decides (D5a).
            emit_import_edge(self.project, self.emitter, &resolved, &resolved, range, false);
        }
    }

    /// Which dynamic-import function `function` (a call's callee) statically
    /// is, if either (D1).
    fn dynamic_import_callee(&self, function: Node) -> Option<DynamicCallee> {
        match function.kind() {
            "identifier" => {
                let name = text(function, self.source);
                if self.binding_is_importlib(name, ImportlibName::ImportModule) {
                    Some(DynamicCallee::ImportModule)
                } else {
                    (name == "__import__" && self.unbound(name)).then_some(DynamicCallee::DunderImport)
                }
            }
            "attribute" => match dotted_segments(function, self.source)?.as_slice() {
                [head, "import_module"] => self
                    .binding_is_importlib(head, ImportlibName::Module)
                    .then_some(DynamicCallee::ImportModule),
                _ => None,
            },
            _ => None,
        }
    }

    /// Whether `name`, used here, is bound to the `importlib` spelling
    /// `want` (D1, D7b).
    ///
    /// The first enclosing function frame that declares or binds the name
    /// decides: a nested `def` of it means no, and a local means yes only when
    /// the pre-scan found every binding of it to be that one import. With no
    /// such frame, the module's own binding decides: its final binding must
    /// be an import (no declaration after it), still `External`, and recorded
    /// as that spelling.
    fn binding_is_importlib(&self, name: &str, want: ImportlibName) -> bool {
        for frame in self.scopes.chain() {
            if frame.kind() == FrameKind::Module {
                if frame.binds(name) {
                    return false;
                }
                break;
            }
            if self.model.declares_name(frame.path(), name) {
                return false;
            }
            if frame.binds(name) {
                return frame.importlib(name) == Some(want);
            }
        }
        if !matches!(self.model.module_binding(name, None), None | Some(ModuleBinding::Import(_))) {
            return false;
        }
        self.model.lookup_import(name) == Some(&Import::External) && self.model.importlib(name) == Some(want)
    }

    /// Whether nothing in this file binds `name`: no frame binds or declares
    /// it, the module does not import it, and no module-level binding of any
    /// form ([`FileModel::rebound`]) takes it. What makes a bare
    /// `__import__`/`__package__`/`__name__` the builtin.
    fn unbound(&self, name: &str) -> bool {
        for frame in self.scopes.chain() {
            if frame.binds(name) || self.model.declares_name(frame.path(), name) {
                return false;
            }
        }
        self.model.lookup_import(name).is_none() && !self.model.rebound(name)
    }

    /// The string `node` statically is, from this file alone (D2): a plain
    /// string literal, or a name that resolves to a module-level constant of
    /// this file (D3). A name some enclosing function or class binds or
    /// declares is that local, not the constant.
    fn fold(&self, node: Node) -> Option<String> {
        match node.kind() {
            "string" => plain_string(node, self.source).map(str::to_string),
            "identifier" => {
                let name = text(node, self.source);
                for frame in self.scopes.chain() {
                    if frame.kind() == FrameKind::Module {
                        break;
                    }
                    if frame.binds(name) || self.model.declares_name(frame.path(), name) {
                        return None;
                    }
                }
                self.fold_constant(name, &mut Vec::new())
            }
            _ => None,
        }
    }

    /// The value of the module-level constant `name`, following `A = B`
    /// chains; `seen` breaks an `A = B`, `B = A` cycle.
    fn fold_constant(&self, name: &str, seen: &mut Vec<String>) -> Option<String> {
        if seen.iter().any(|earlier| earlier == name) {
            return None;
        }
        seen.push(name.to_string());
        let Some(ModuleBinding::Decl(decl)) = self.model.module_binding(name, None) else { return None };
        if decl.kind != NodeKind::Variable {
            return None;
        }
        match self.model.constant(&decl.id, name)? {
            ConstValue::Literal(value) => Some(value.clone()),
            ConstValue::Name(next) => self.fold_constant(next, seen),
        }
    }

    /// The `package` argument of `import_module`, and whether it is this
    /// file's own package (D5): `__package__` is the package a relative
    /// import here resolves against, `__name__` this module's own key (an
    /// orphan has neither). Anything else folds as a string ([`Bodies::fold`]).
    fn fold_package(&self, node: Node) -> Option<(String, bool)> {
        if node.kind() == "identifier" {
            let name = text(node, self.source);
            if matches!(name, "__package__" | "__name__") && self.unbound(name) {
                if self.module.role == FileRole::Orphan {
                    return None;
                }
                return if name == "__package__" {
                    self.module.package.clone().map(|package| (package, true))
                } else {
                    Some((self.module.key.clone(), true))
                };
            }
        }
        self.fold(node).map(|value| (value, false))
    }

    // --- name resolution ------------------------------------------------------

    /// A single-segment name used as a value, a type or a callee.
    fn bare(&mut self, node: Node, from: &str, is_call: bool) {
        let name = text(node, self.source);
        #[cfg(test)]
        crate::census::push_call(is_call);
        let bound = self.resolve_bare(name, None);
        #[cfg(test)]
        crate::census::pop_call();
        self.emit(bound, from, node, is_call, node);
    }

    /// A dotted name - `mod.helper`, `Cls.method`, `self.render`,
    /// `obj.method`.
    ///
    /// `access` says whether this use reads, writes or deletes the member; it
    /// only matters for an instance-parameter use of a property with a
    /// setter/deleter ([`Bodies::routed_accessor`]).
    fn attribute(&mut self, node: Node, from: &str, is_call: bool, access: Access) {
        let Some(segments) = dotted_segments(node, self.source) else {
            // Not a chain of plain names: `factory().run`, `rows[0].run`. The
            // object is still real code, and a call on it is a receiver call
            // like any other.
            self.walk_receiver(node, from, is_call);
            return;
        };
        #[cfg(test)]
        crate::census::push_call(is_call);
        let bound = self.resolve_path(&segments);
        #[cfg(test)]
        crate::census::pop_call();
        // The *name* position is where a placeholder's range and an open
        // site's position belong - the last segment, not the whole chain.
        let at = node.child_by_field_name("attribute").unwrap_or(node);
        match bound {
            Bound::Receiver => self.walk_receiver(node, from, is_call),
            Bound::Here(getter, kind) => match (self.routed_accessor(&segments, access), access) {
                // Read, then write: the getter and the setter.
                (Some(setter), Access::Augmented) => {
                    self.emit(Bound::Here(getter, kind), from, node, is_call, at);
                    self.emit(Bound::Here(setter.id, setter.kind), from, node, is_call, at);
                }
                (Some(routed), _) => self.emit(Bound::Here(routed.id, routed.kind), from, node, is_call, at),
                (None, _) => self.emit(Bound::Here(getter, kind), from, node, is_call, at),
            },
            other => self.emit(other, from, node, is_call, at),
        }
    }

    /// The setter (a store, an augmented assignment) or deleter (a `del`)
    /// `segments` runs, when it is an instance-parameter use (`self.x`) of a
    /// property this file declares that accessor for. `None` for a load, for
    /// any other dotted name - `C.x = v` on the class replaces the
    /// descriptor rather than calling its setter - and for a property
    /// without that accessor, whose store keeps referencing the getter.
    fn routed_accessor(&self, segments: &[&str], access: Access) -> Option<DeclRef> {
        let accessor = match access {
            Access::Load => return None,
            Access::Store | Access::Augmented => Accessor::Setter,
            Access::Delete => Accessor::Deleter,
        };
        let qualified = self.instance_member(segments)?;
        self.model.accessor(&qualified, accessor).cloned()
    }

    /// The qualified name `self.member` (whatever the first parameter is
    /// really called) addresses in the enclosing method's class - see the
    /// module doc, Decision 7. `None` for anything that is not exactly the
    /// instance parameter and one member.
    fn instance_member(&self, segments: &[&str]) -> Option<String> {
        let instance = self.instance.as_ref()?;
        match segments {
            [head, member] if *head == instance.parameter => {
                Some(format!("{}.{member}", instance.class_path))
            }
            _ => None,
        }
    }

    /// An attribute whose receiver this tier does not know: walk the receiver
    /// (it is ordinary code), and for a *call* record the one open site this
    /// plugin emits.
    fn walk_receiver(&mut self, node: Node, from: &str, is_call: bool) {
        if let Some(object) = node.child_by_field_name("object") {
            self.visit(object, from);
        }
        if !is_call {
            return;
        }
        let Some(field) = node.child_by_field_name("attribute") else { return };
        let position = self.emitter.positions().at(field.start_position());
        self.emitter.open_site(OpenSite {
            from_id: from.to_string(),
            position,
            name: text(field, self.source).to_string(),
            kind: OpenSiteKind::ReceiverCall,
            edge_kind: EdgeKind::Calls,
            from_container: Some(self.module.key.clone()),
            // Always `None` here, for the same reason `plugins/rust` gives:
            // the field names a structural edge a semantic answer would
            // contradict, and this extractor writes no edge at all for a
            // receiver call (Decision 7 in this module's doc). There is
            // nothing to retract, so claiming otherwise would ask the bridge
            // to delete an edge that was never emitted.
            replaces: None,
        });
    }

    /// Python's own LEGB walk, minus builtins: the nearest enclosing scope
    /// that declares or binds the name wins, and a class scope that is not
    /// the innermost one is skipped ([`super::scope`]).
    ///
    /// In the module frame the name means the module's final binding of it
    /// ([`FileModel::module_binding`]): a declaration an unconditional named
    /// import written after it displaced is addressed through that import,
    /// and one a later `*` import may rebind is addressed at this module's
    /// own container, the address `from pkg import f` uses elsewhere, so the
    /// linker decides between the star's provider and the declaration.
    fn resolve_bare(&self, name: &str, want: Option<NodeKind>) -> Bound {
        for frame in self.scopes.chain() {
            if frame.path().is_empty() {
                match self.model.module_binding(name, want) {
                    Some(ModuleBinding::Decl(decl)) => return Bound::Here(decl.id.clone(), decl.kind),
                    Some(ModuleBinding::DeclBeforeStar(decl)) => {
                        return Bound::There {
                            target: container_target(
                                &self.module.key,
                                TargetKey::Name(name.to_string()),
                                &self.module.key,
                            ),
                            name: name.to_string(),
                            looks_class: decl.kind == NodeKind::Type,
                        };
                    }
                    Some(ModuleBinding::Import(_)) => break,
                    None => {}
                }
            } else if let Some(decl) = self.model.lookup(frame.path(), name, want) {
                return Bound::Here(decl.id.clone(), decl.kind);
            }
            if frame.binds(name) {
                #[cfg(test)]
                crate::census::record(crate::census::Reason::BareLocal, name);
                return Bound::Nothing;
            }
        }
        #[cfg(test)]
        match self.model.lookup_import(name) {
            Some(Import::Module { .. }) => {
                crate::census::record(crate::census::Reason::BareImportedModule, name)
            }
            Some(Import::External) => crate::census::record(crate::census::Reason::BareExternalImport, name),
            None => crate::census::record(crate::census::Reason::BareUnknown, name),
            Some(Import::Item { .. }) => {}
        }
        match self.model.lookup_import(name) {
            Some(Import::Item { container, name }) => Bound::There {
                target: container_target(container, TargetKey::Name(name.clone()), &self.module.key),
                name: name.clone(),
                looks_class: looks_like_class(name),
            },
            // A bare use of a module name (`import a.b` then `a` on its own).
            // The module's own node is a member of *its* package, not of this
            // file's container, and a top-level package has no such node at
            // all - so there is nothing here to address. An external import,
            // and a name nothing here knows (a builtin, a star-import
            // member), are the same answer for different reasons.
            Some(Import::Module { .. }) | Some(Import::External) | None => Bound::Nothing,
        }
    }

    /// A multi-segment dotted name.
    fn resolve_path(&self, segments: &[&str]) -> Bound {
        let Some((last, head)) = segments.split_last() else { return Bound::Nothing };
        if head.is_empty() {
            return self.resolve_bare(last, None);
        }
        // `self.member` (whatever the first parameter is really called) -
        // see the module doc, Decision 7.
        if let Some(qualified) = self.instance_member(segments) {
            return match self.model.lookup_qualified(&qualified) {
                Some(decl) => Bound::Here(decl.id.clone(), decl.kind),
                // Inherited from a base class, or an attribute rather
                // than a method: not something this file declares, so
                // not something this tier may name.
                None => Bound::Receiver,
            };
        }
        match self.resolve_qualifier(head) {
            Qualifier::Container(container) => {
                if container == self.module.key {
                    if let Some(decl) = self.model.lookup("", last, None) {
                        return Bound::Here(decl.id.clone(), decl.kind);
                    }
                }
                Bound::There {
                    target: container_target(
                        &container,
                        TargetKey::Name((*last).to_string()),
                        &self.module.key,
                    ),
                    name: (*last).to_string(),
                    looks_class: looks_like_class(last),
                }
            }
            Qualifier::Declaration { container, qualified } => {
                let full = format!("{qualified}.{last}");
                if container == self.module.key {
                    return match self.model.lookup_qualified(&full) {
                        Some(decl) => Bound::Here(decl.id.clone(), decl.kind),
                        // A member of one of this file's own classes that
                        // this file does not declare - an inherited method,
                        // or a class attribute (which is not a node here, see
                        // `super::decls`). A placeholder addressed at our own
                        // container could never be answered by anything, so
                        // emitting one would be litter, not an edge.
                        None => {
                            #[cfg(test)]
                            crate::census::record(crate::census::Reason::DottedOwnMissing, last);
                            Bound::Nothing
                        }
                    };
                }
                let key_path = dotted_path(&full);
                let mut target =
                    container_target(&container, TargetKey::QualifiedName(full), &self.module.key);
                target.key_path = key_path;
                Bound::There { target, name: (*last).to_string(), looks_class: looks_like_class(last) }
            }
            Qualifier::Opaque => Bound::Receiver,
            Qualifier::External => {
                #[cfg(test)]
                crate::census::record(crate::census::Reason::DottedExternal, last);
                Bound::Nothing
            }
        }
    }

    /// What the prefix of a dotted name names - a module, a declaration, or
    /// something opaque.
    fn resolve_qualifier(&self, head: &[&str]) -> Qualifier {
        let Some((first, rest)) = head.split_first() else { return Qualifier::Opaque };
        let mut qualifier = self.qualifier_of(first);
        for segment in rest {
            qualifier = qualifier.extend(segment);
        }
        qualifier
    }

    /// What the bare `name` heading a dotted name names. In the module frame
    /// it is the module's final binding of it, as in
    /// [`Bodies::resolve_bare`], except that a declaration a later `*` import
    /// may rebind stays the declaration: a `qualifiedName` key never follows
    /// a re-export row, so there is nothing for the linker to decide.
    fn qualifier_of(&self, name: &str) -> Qualifier {
        for frame in self.scopes.chain() {
            let declared = if frame.path().is_empty() {
                match self.model.module_binding(name, None) {
                    Some(ModuleBinding::Import(_)) => break,
                    binding => binding.is_some(),
                }
            } else {
                self.model.lookup(frame.path(), name, None).is_some()
            };
            if declared {
                let qualified = if frame.path().is_empty() {
                    name.to_string()
                } else {
                    format!("{}.{name}", frame.path())
                };
                return Qualifier::Declaration { container: self.module.key.clone(), qualified };
            }
            if frame.binds(name) {
                return Qualifier::Opaque;
            }
        }
        match self.model.lookup_import(name) {
            Some(Import::Module { container }) => Qualifier::Container(container.clone()),
            // `from . import helpers` then `helpers.assist()`, versus
            // `from .mod import Greeter` then `Greeter.build()`. Which of the
            // two the name is, only the import system knows; the naming
            // convention chooses between two *addresses* and never between
            // emitting an edge and not - see `super::syntax::looks_like_class`.
            Some(Import::Item { container, name }) => {
                if looks_like_class(name) {
                    Qualifier::Declaration { container: container.clone(), qualified: name.clone() }
                } else {
                    Qualifier::Container(format!("{container}.{name}"))
                }
            }
            Some(Import::External) => Qualifier::External,
            None => Qualifier::Opaque,
        }
    }

    // --- emission -------------------------------------------------------------

    /// `at` is the node a placeholder's range points at - the *name* position,
    /// which for a dotted chain is its last segment; `whole` is the node whose
    /// receiver would be walked if this turned out to be one.
    fn emit(&mut self, bound: Bound, from: &str, whole: Node, is_call: bool, at: Node) {
        match bound {
            Bound::Here(to, kind) => {
                // Core's linker lands a `CALLS` edge only on a `Function`, and
                // `Greeter()` is a use of a class rather than a call to a
                // function - so the node's own kind decides, exactly.
                let edge = if is_call && kind == NodeKind::Function {
                    EdgeKind::Calls
                } else {
                    EdgeKind::References
                };
                let written = self.emitter.resolved_edge(edge, from, &to);
                if edge == EdgeKind::Calls && self.emitter.overloaded(&to) {
                    self.overload_call(from, written, at);
                }
            }
            Bound::There { target, name, looks_class } => {
                let edge = if is_call && !looks_class { EdgeKind::Calls } else { EdgeKind::References };
                let range = self.emitter.positions().range(at);
                let placeholder =
                    self.emitter.placeholder(PlaceholderKind::PendingSymbol, &name, target, range);
                let written = self.emitter.placeholder_edge(edge, from, &placeholder);
                // Whether the other file's declaration is an overload set is
                // not knowable here; the engine filters (Decision 7).
                if edge == EdgeKind::Calls {
                    self.overload_call(from, written, at);
                }
            }
            Bound::Receiver => self.walk_receiver(whole, from, is_call),
            Bound::Nothing => {}
        }
    }

    /// Records the [`OpenSiteKind::OverloadCall`] site of a call whose
    /// structural `CALLS` edge `replaces` (from `from`) was just written - see
    /// Decision 7's overloaded calls. The engine either binds every site of
    /// that edge to an overload or leaves the edge be.
    fn overload_call(&mut self, from: &str, replaces: String, at: Node) {
        let position = self.emitter.positions().at(at.start_position());
        self.emitter.open_site(OpenSite {
            from_id: from.to_string(),
            position,
            name: text(at, self.source).to_string(),
            kind: OpenSiteKind::OverloadCall,
            edge_kind: EdgeKind::Calls,
            from_container: Some(self.module.key.clone()),
            replaces: Some(replaces),
        });
    }
}
