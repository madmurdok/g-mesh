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

use crate::extractor::emit::{container_target, Emitter};
use crate::extractor::keys::ModuleCtx;
use crate::extractor::model::{FileModel, Import};
use crate::extractor::scope::{FrameKind, Scopes};
use crate::extractor::syntax::{
    decorators, definition_name, dotted_segments, first_parameter_name, has_decorator, inner_definition,
    looks_like_class, text,
};

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

/// Walks a file's bodies against what [`Declarer`](super::decls::Declarer)
/// found.
///
/// It deliberately holds no [`ProjectContext`](crate::project::ProjectContext):
/// the project model answers questions about *other* files (which dotted
/// names exist, where a root is), and every question this pass asks is about
/// names the file itself established - its own declarations, and what its own
/// import statements bound. `super::decls` is where the project model is
/// consulted, once per import.
pub(crate) struct Bodies<'a, 's> {
    pub(crate) module: &'a ModuleCtx,
    pub(crate) source: &'s str,
    pub(crate) model: &'a FileModel,
    pub(crate) emitter: &'a mut Emitter<'s>,
    pub(crate) scopes: Scopes,
    instance: Option<Instance>,
}

impl<'a, 's> Bodies<'a, 's> {
    pub(crate) fn new(
        module: &'a ModuleCtx,
        source: &'s str,
        model: &'a FileModel,
        emitter: &'a mut Emitter<'s>,
    ) -> Self {
        Self { module, source, model, emitter, scopes: Scopes::new(), instance: None }
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
            // Pass 1 read these; walking them again would emit a second,
            // duplicate reference for every imported name.
            "import_statement" | "import_from_statement" => {}
            // `global x` / `nonlocal x` name a binding, not a use of one.
            "global_statement" | "nonlocal_statement" => {}
            "comment" => {}
            "call" => self.call(node, from),
            "attribute" => self.attribute(node, from, false),
            "identifier" => self.bare(node, from, false),
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
        let id = self
            .model
            .lookup_qualified(&qualified)
            .map(|decl| decl.id.clone())
            .unwrap_or_else(|| from.to_string());

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
                Bound::Here(to, _) => self.emitter.resolved_edge(EdgeKind::SupertypeOf, subtype, &to),
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
        if let Some(left) = node.child_by_field_name("left") {
            self.visit_target(left, from);
        }
    }

    fn visit_target(&mut self, target: Node, from: &str) {
        match target.kind() {
            "identifier" => {}
            "attribute" | "subscript" => self.visit(target, from),
            _ => {
                let mut cursor = target.walk();
                let children: Vec<Node> = target.named_children(&mut cursor).collect();
                for child in children {
                    self.visit_target(child, from);
                }
            }
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
            "attribute" => self.attribute(function, from, true),
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
    fn attribute(&mut self, node: Node, from: &str, is_call: bool) {
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
            other => self.emit(other, from, node, is_call, at),
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
    fn resolve_bare(&self, name: &str, want: Option<NodeKind>) -> Bound {
        for frame in self.scopes.chain() {
            if let Some(decl) = self.model.lookup(frame.path(), name, want) {
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
        if let Some(instance) = &self.instance {
            if head.len() == 1 && head[0] == instance.parameter {
                let qualified = format!("{}.{last}", instance.class_path);
                return match self.model.lookup_qualified(&qualified) {
                    Some(decl) => Bound::Here(decl.id.clone(), decl.kind),
                    // Inherited from a base class, or an attribute rather
                    // than a method: not something this file declares, so
                    // not something this tier may name.
                    None => Bound::Receiver,
                };
            }
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
                Bound::There {
                    target: container_target(&container, TargetKey::QualifiedName(full), &self.module.key),
                    name: (*last).to_string(),
                    looks_class: looks_like_class(last),
                }
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

    fn qualifier_of(&self, name: &str) -> Qualifier {
        for frame in self.scopes.chain() {
            if self.model.lookup(frame.path(), name, None).is_some() {
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
                self.emitter.resolved_edge(edge, from, &to);
            }
            Bound::There { target, name, looks_class } => {
                let edge = if is_call && !looks_class { EdgeKind::Calls } else { EdgeKind::References };
                let range = self.emitter.positions().range(at);
                let placeholder =
                    self.emitter.placeholder(PlaceholderKind::PendingSymbol, &name, target, range);
                self.emitter.placeholder_edge(edge, from, &placeholder);
            }
            Bound::Receiver => self.walk_receiver(whole, from, is_call),
            Bound::Nothing => {}
        }
    }
}
