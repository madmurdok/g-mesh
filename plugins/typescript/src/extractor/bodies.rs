//! Function bodies and the names they use: the lexical scope chain, calls and
//! references.
//!
//! - **Scope.** Every function pushes its parameters and hoisted `var`s and
//!   function declarations; every block, catch clause and `for` header its
//!   own bindings ([`crate::extractor::scope`]). A name the chain binds is a
//!   local, never this file's symbol of the same spelling, so it produces no
//!   edge.
//! - **Callers.** A call is attributed to the nearest enclosing function
//!   node, or inside an unnamed function to the nearest enclosing declared
//!   symbol. At module top level nothing makes the call, and it degrades to
//!   a `REFERENCES` edge from the `File`.
//! - **Targets.** `f()` binds the `Function` an unqualified name reaches
//!   (innermost namespace outwards), else the `pending_symbol` placeholder of
//!   an import. `this.m()` binds a member of the enclosing type, `super.m()`
//!   one of a supertype declared here, `Owner.m()` and `new Owner()` one of a
//!   type or namespace declared here; a `this.m()` / `super.m()` that binds
//!   nothing here is a receiver-call open site. Class members are never
//!   reached by bare name. A `CALLS` edge always targets a `Function` or a `pending_symbol`;
//!   any other target is a `REFERENCES` edge.
//! - **Heritage.** Each name a class's `extends`/`implements` or an
//!   interface's `extends` writes is a `SUPERTYPE_OF` edge from the subtype
//!   to the type it reaches, else to an import's placeholder. Only the
//!   clause's type arguments are walked as uses.
//! - **Order.** Uses resolve after the walk: supertypes, then calls, then
//!   references (a name already called from a symbol is not also referenced
//!   by it), then member accesses. The open sites they leave are
//!   [`crate::extractor::sites`]'.

use std::collections::HashSet;

use g_mesh_plugin_sdk::wire::{EdgeKind, NodeKind};
use tree_sitter::Node;

use crate::extractor::decls::Declarer;
use crate::extractor::model::{
    CallReceiver, CallSite, PendingCall, PendingMemberAccess, PendingReference, PendingSupertype,
};
use crate::extractor::scope::{
    bound_scope, collect_declaration_names, collect_pattern_names, declares_binding, function_scope,
    is_binding_position, is_locally_bound, type_parameter_scope, Scope,
};
use crate::extractor::syntax::named_children;

/// The supertypes, calls, references and member accesses a walk records,
/// resolved once it is over, and the call sites resolution produces.
#[derive(Debug, Default)]
pub struct UseState<'t> {
    pub(super) supertypes: Vec<PendingSupertype<'t>>,
    calls: Vec<PendingCall<'t>>,
    references: Vec<PendingReference<'t>>,
    pub(super) member_accesses: Vec<PendingMemberAccess<'t>>,
    /// Every call that produced a `CALLS` edge, in resolution order.
    pub(super) call_sites: Vec<CallSite>,
    /// The edges onto a `pending_symbol` placeholder that already have a
    /// hop site: one question per edge, at its first use.
    pub(super) hop_edges: HashSet<String>,
}

impl<'a, 's, 't> Declarer<'a, 's, 't> {
    // --- function parts ----------------------------------------------------

    /// Walks a declared function's parts with the function as the enclosing
    /// symbol and caller.
    pub(super) fn visit_function_body(&mut self, node: Node<'t>, scope: &Scope, function: usize) {
        let function = self.model.node(function);
        let inner = Scope {
            prefix: function.qualified_path.clone().unwrap_or_default(),
            enclosing_caller_id: Some(function.id.clone()),
            enclosing_symbol_id: function.id.clone(),
            inside_function: true,
            ..scope.clone()
        };
        self.visit_function_parts(node, &inner);
    }

    /// Walks any function's type parameters, parameters, return type and
    /// body as the inside of a function, with its parameters and hoisted
    /// declarations bound.
    pub(super) fn visit_function_parts(&mut self, node: Node<'t>, scope: &Scope) {
        let enclosing_caller_id = scope.enclosing_caller_id.clone().or_else(|| self.caller_fallback(scope));
        let body_scope = Scope {
            inside_function: true,
            locals: function_scope(node, self.source, scope.locals.as_ref()),
            enclosing_caller_id,
            ..type_parameter_scope(node, self.source, scope.clone())
        };
        self.visit_field(node, "type_parameters", &body_scope);
        if let Some(parameters) = node.child_by_field_name("parameters") {
            self.visit_parameters(parameters, &body_scope);
        }
        self.visit_field(node, "return_type", &body_scope);
        self.visit_field(node, "body", &body_scope);
    }

    /// The `from` of a call inside an unnamed function: the nearest enclosing
    /// declared symbol, never the `File`.
    fn caller_fallback(&self, scope: &Scope) -> Option<String> {
        (scope.enclosing_symbol_id != self.model.file_id()).then(|| scope.enclosing_symbol_id.clone())
    }

    /// A parameter list's types and default values, including the defaults
    /// and computed keys nested in destructuring patterns; the binding names
    /// themselves are bindings, not uses.
    pub(super) fn visit_parameters(&mut self, node: Node<'t>, scope: &Scope) {
        for parameter in named_children(node) {
            match parameter.kind() {
                "required_parameter" | "optional_parameter" => {
                    self.visit_field(parameter, "type", scope);
                    if let Some(pattern) = parameter.child_by_field_name("pattern") {
                        self.visit_binding_pattern(pattern, scope);
                    }
                    self.visit_field(parameter, "value", scope);
                }
                // The JavaScript grammar writes parameters as bare patterns.
                _ => self.visit_binding_pattern(parameter, scope),
            }
        }
    }

    /// The expressions inside a binding pattern: default values
    /// (`{ y = d() }`, `[y = d()]`, `y = d`) and computed keys
    /// (`{ [k]: y }`). The names it binds are skipped.
    fn visit_binding_pattern(&mut self, node: Node<'t>, scope: &Scope) {
        match node.kind() {
            "assignment_pattern" | "object_assignment_pattern" => {
                if let Some(left) = node.child_by_field_name("left") {
                    self.visit_binding_pattern(left, scope);
                }
                self.visit_field(node, "right", scope);
            }
            "pair_pattern" => {
                if let Some(key) = node.child_by_field_name("key") {
                    if key.kind() == "computed_property_name" {
                        self.visit(key, scope);
                    }
                }
                if let Some(value) = node.child_by_field_name("value") {
                    self.visit_binding_pattern(value, scope);
                }
            }
            "object_pattern" | "array_pattern" | "rest_pattern" => {
                for child in named_children(node) {
                    self.visit_binding_pattern(child, scope);
                }
            }
            _ => {}
        }
    }

    /// `catch (err) { ... }`: `err` is bound for the handler only.
    pub(super) fn visit_catch_clause(&mut self, node: Node<'t>, scope: &Scope) {
        let parameter = node.child_by_field_name("parameter");
        let mut names = HashSet::new();
        if let Some(parameter) = parameter {
            collect_pattern_names(parameter, self.source, &mut names);
        }
        let inner = bound_scope(names, scope);
        for child in named_children(node) {
            if Some(child.id()) != parameter.map(|parameter| parameter.id()) {
                self.visit(child, &inner);
            }
        }
    }

    /// `for (const x of xs)` / `for (let i = 0; ...)`: the loop variables are
    /// bound for the header and the body. The subject of a `for...of`/`in`
    /// is evaluated outside them, and `for (existing of xs)` binds nothing.
    pub(super) fn visit_for_statement(&mut self, node: Node<'t>, scope: &Scope) {
        let mut names = HashSet::new();
        let left = node.child_by_field_name("left");
        if let Some(left) = left.filter(|_| declares_binding(node)) {
            collect_pattern_names(left, self.source, &mut names);
        }
        if let Some(initializer) = node.child_by_field_name("initializer") {
            collect_declaration_names(initializer, self.source, &mut names);
        }
        let inner = bound_scope(names, scope);
        let right = node.child_by_field_name("right");
        if let Some(right) = right {
            self.visit(right, scope);
        }
        for child in named_children(node) {
            if Some(child.id()) == left.map(|left| left.id())
                || Some(child.id()) == right.map(|right| right.id())
            {
                continue;
            }
            self.visit(child, &inner);
        }
    }

    // --- recording uses ------------------------------------------------------

    fn record_call(&mut self, at: Node<'t>, receiver: CallReceiver, scope: &Scope) {
        self.record_named_call(self.text(at), at, receiver, scope);
    }

    fn record_named_call(&mut self, name: &str, at: Node<'t>, receiver: CallReceiver, scope: &Scope) {
        self.uses.calls.push(PendingCall { name: name.to_string(), receiver, scope: scope.clone(), at });
    }

    /// A call expression: the call it names, or for `require(...)` and
    /// `import(...)` with a foldable argument a computed import, then its
    /// type arguments and arguments.
    ///
    /// A receiver that is neither `this`, `super` nor a bare identifier
    /// (`a.b.c()`, `f().g()`) needs a type to resolve: the receiver is walked
    /// and the call is a receiver-call open site.
    pub(super) fn handle_call(&mut self, node: Node<'t>, scope: &Scope) {
        if let Some(callee) = node.child_by_field_name("function") {
            match callee.kind() {
                "identifier" => {
                    let name = self.text(callee);
                    // A `require(...)` that is not read as an import is a call
                    // of the name `require`, which a file may declare itself.
                    if name != "require" || !self.record_call_import(node, scope, Some(callee)) {
                        self.record_call(callee, CallReceiver::None, scope);
                    }
                }
                "import" => {
                    self.record_call_import(node, scope, None);
                }
                "super" => self.record_named_call("constructor", callee, CallReceiver::Super, scope),
                "member_expression" => {
                    let object = callee.child_by_field_name("object");
                    let property = callee.child_by_field_name("property");
                    match (object, property) {
                        (Some(object), Some(property)) if matches!(object.kind(), "this" | "super") => {
                            let receiver = if object.kind() == "this" {
                                CallReceiver::This
                            } else {
                                CallReceiver::Super
                            };
                            self.record_call(property, receiver, scope);
                        }
                        (Some(object), Some(property)) if object.kind() == "identifier" => {
                            let receiver = CallReceiver::Qualified(self.text(object).to_string());
                            self.record_call(property, receiver, scope);
                            self.record_member_access(object, property, scope, true);
                        }
                        (Some(object), property) => {
                            self.visit(object, scope);
                            if let Some(property) = property {
                                self.record_receiver_call(property, scope);
                            }
                        }
                        _ => {}
                    }
                }
                _ => self.visit(callee, scope),
            }
        }
        self.visit_field(node, "type_arguments", scope);
        if let Some(arguments) = node.child_by_field_name("arguments") {
            self.visit_children(arguments, scope);
        }
    }

    /// `new Owner(...)` is a call of `Owner`'s constructor.
    pub(super) fn handle_new(&mut self, node: Node<'t>, scope: &Scope) {
        match node.child_by_field_name("constructor") {
            Some(constructor) if constructor.kind() == "identifier" => {
                let receiver = CallReceiver::New(self.text(constructor).to_string());
                self.record_named_call("constructor", constructor, receiver, scope);
            }
            Some(constructor) => self.visit(constructor, scope),
            None => {}
        }
        self.visit_field(node, "type_arguments", scope);
        if let Some(arguments) = node.child_by_field_name("arguments") {
            self.visit_children(arguments, scope);
        }
    }

    /// `obj.prop` outside a call's callee: walked like any expression, and
    /// kept as a member access in case `obj` is a namespace import.
    pub(super) fn handle_member_expression(&mut self, node: Node<'t>, scope: &Scope) {
        let object = node.child_by_field_name("object");
        let property = node.child_by_field_name("property");
        if let (Some(object), Some(property)) = (object, property) {
            if object.kind() == "identifier" {
                self.record_member_access(object, property, scope, false);
            }
        }
        self.visit_children(node, scope);
    }

    fn record_member_access(&mut self, object: Node<'t>, property: Node<'t>, scope: &Scope, is_call: bool) {
        self.uses.member_accesses.push(PendingMemberAccess {
            object_name: self.text(object).to_string(),
            at: property,
            scope: scope.clone(),
            is_call,
        });
    }

    /// An identifier outside a binding position is a use of its name.
    pub(super) fn record_reference(&mut self, node: Node<'t>, scope: &Scope) {
        if is_binding_position(node) {
            return;
        }
        self.uses.references.push(PendingReference {
            name: self.text(node).to_string(),
            scope: scope.clone(),
            at: node,
            type_position: node.kind() == "type_identifier",
        });
    }

    /// The `require` callees whose argument did not fold, as calls of
    /// `require`, after every call the walk recorded.
    pub(super) fn record_require_calls(&mut self, callees: Vec<(Node<'t>, Scope)>) {
        for (callee, scope) in callees {
            self.record_call(callee, CallReceiver::None, &scope);
        }
    }

    // --- resolving uses -------------------------------------------------------

    /// Resolves every recorded supertype, call, reference and member access,
    /// in that order.
    pub(super) fn resolve_uses(&mut self) {
        for supertype in std::mem::take(&mut self.uses.supertypes) {
            self.resolve_supertype(&supertype);
        }
        for call in std::mem::take(&mut self.uses.calls) {
            self.resolve_call(&call);
        }
        for reference in std::mem::take(&mut self.uses.references) {
            self.resolve_reference(&reference.name, &reference.scope, reference.type_position, reference.at);
        }
        for access in std::mem::take(&mut self.uses.member_accesses) {
            self.collect_namespace_member_use(&access);
        }
    }

    /// `SUPERTYPE_OF` runs from the subtype to the supertype, so a type's
    /// implementations are its inbound edges.
    fn resolve_supertype(&mut self, supertype: &PendingSupertype<'t>) {
        let target = self.lookup_type(&supertype.name, &supertype.scope);
        if let Some(target) = target.or_else(|| self.imported_symbol(&supertype.name)) {
            let target_id = self.model.node(target).id.clone();
            self.model.add_edge(&supertype.from_id, EdgeKind::SupertypeOf, &target_id);
            self.record_placeholder_use_site(
                &supertype.from_id,
                EdgeKind::SupertypeOf,
                &target_id,
                supertype.at,
            );
        }
    }

    fn resolve_call(&mut self, call: &PendingCall<'t>) {
        // A locally bound name is not a graph symbol. `this.m()`/`super.m()`
        // name a member, which no local shadows.
        let shadowable = match &call.receiver {
            CallReceiver::None => Some(call.name.as_str()),
            receiver => receiver.owner(),
        };
        let shadowed = shadowable.is_some_and(|name| is_locally_bound(name, call.scope.locals.as_ref()));

        // `obj.m()` that reaches no member declared here is a receiver call,
        // whatever `obj` is, unless `obj` is a namespace import: that site is
        // a question of its own.
        if let CallReceiver::Qualified(object) = &call.receiver {
            if (shadowed || self.lookup_call_target(call).is_none())
                && !self.is_namespace_receiver(object, &call.scope)
            {
                self.record_receiver_call(call.at, &call.scope);
            }
        }
        if shadowed {
            return;
        }

        if let Some(target) = self.lookup_call_target(call) {
            match &call.scope.enclosing_caller_id {
                Some(caller) if self.model.node(target).kind == NodeKind::Function => {
                    self.add_call(caller, target, call.at);
                }
                _ => self.add_usage(target, &call.scope, call.at),
            }
            return;
        }
        match &call.receiver {
            CallReceiver::None => match self.imported_symbol(&call.name) {
                Some(imported) => match &call.scope.enclosing_caller_id {
                    Some(caller) => self.add_call(caller, imported, call.at),
                    None => self.add_usage(imported, &call.scope, call.at),
                },
                None => self.resolve_reference(&call.name, &call.scope, false, call.at),
            },
            // Which member of an imported receiver is the semantic tier's
            // question; the receiver itself is a use.
            CallReceiver::Qualified(object) | CallReceiver::New(object) => {
                self.resolve_reference(object, &call.scope, false, receiver_token(call))
            }
            // A member of a type this file does not declare (a base class in
            // another file, an object literal's method): a question for the
            // semantic tier. `super(...)` names no member and is not one.
            CallReceiver::This | CallReceiver::Super => {
                if call.at.kind() != "super" {
                    self.record_receiver_call(call.at, &call.scope);
                }
            }
        }
    }

    fn lookup_call_target(&self, call: &PendingCall<'t>) -> Option<usize> {
        let scope = &call.scope;
        match &call.receiver {
            CallReceiver::None => {
                self.model.lookup_by_name(&call.name, &scope.namespace_prefix, Some(NodeKind::Function))
            }
            CallReceiver::This => {
                scope.enclosing_type_qname.as_deref().and_then(|owner| self.lookup_member(owner, &call.name))
            }
            CallReceiver::Super => scope.supertype_names.iter().find_map(|supertype| {
                let supertype = self.lookup_type(supertype, scope)?;
                self.lookup_member(&self.model.node(supertype).qualified_name, &call.name)
            }),
            CallReceiver::Qualified(object) | CallReceiver::New(object) => {
                if let Some(member) = self.model.lookup_qualified(&format!("{object}.{}", call.name)) {
                    return Some(member);
                }
                let owner = self.model.lookup_by_name(object, &scope.namespace_prefix, None)?;
                let owner = self.model.node(owner);
                if owner.kind == NodeKind::Variable {
                    return None;
                }
                self.lookup_member(&owner.qualified_name, &call.name)
            }
        }
    }

    /// A use of `name`: the symbol an unqualified name reaches, else an
    /// imported one. A type parameter shadows only in a type position.
    fn resolve_reference(&mut self, name: &str, scope: &Scope, type_position: bool, at: Node<'t>) {
        if is_locally_bound(name, scope.locals.as_ref()) {
            return;
        }
        if type_position && is_locally_bound(name, scope.type_parameters.as_ref()) {
            return;
        }
        let target = self.model.lookup_by_name(name, &scope.namespace_prefix, None);
        if let Some(target) = target.or_else(|| self.imported_symbol(name)) {
            self.add_usage(target, scope, at);
        }
    }

    /// The `CALLS` edge from `caller` to node `target`, and the call site
    /// written at `at`.
    fn add_call(&mut self, caller: &str, target: usize, at: Node<'t>) {
        let target_id = self.model.node(target).id.clone();
        self.model.add_edge(caller, EdgeKind::Calls, &target_id);
        self.record_call_site(caller, &target_id, at);
        self.record_placeholder_use_site(caller, EdgeKind::Calls, &target_id, at);
    }

    /// A `REFERENCES` edge from the enclosing symbol, unless it is the
    /// target itself or already calls it; `at` is the name token of the use.
    fn add_usage(&mut self, target: usize, scope: &Scope, at: Node<'t>) {
        let from = scope.enclosing_symbol_id.clone();
        let target_id = self.model.node(target).id.clone();
        if from == target_id || self.model.has_edge(&from, EdgeKind::Calls, &target_id) {
            return;
        }
        self.model.add_edge(&from, EdgeKind::References, &target_id);
        self.record_placeholder_use_site(&from, EdgeKind::References, &target_id, at);
    }

    /// Member `name` of the type `type_qualified_name`, instance or static.
    fn lookup_member(&self, type_qualified_name: &str, name: &str) -> Option<usize> {
        self.model
            .lookup_qualified(&format!("{type_qualified_name}#{name}"))
            .or_else(|| self.model.lookup_qualified(&format!("{type_qualified_name}.{name}")))
    }

    pub(super) fn lookup_type(&self, name: &str, scope: &Scope) -> Option<usize> {
        self.model.lookup_by_name(name, &scope.namespace_prefix, Some(NodeKind::Type))
    }
}

/// The receiver's token of `Owner.m()` (`Owner`) and of `new Owner()`
/// (`Owner`, which is the call's own token).
fn receiver_token<'t>(call: &PendingCall<'t>) -> Node<'t> {
    match call.receiver {
        CallReceiver::Qualified(_) => {
            call.at.parent().and_then(|member| member.child_by_field_name("object")).unwrap_or(call.at)
        }
        _ => call.at,
    }
}
