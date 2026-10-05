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
//!   type or namespace declared here. Class members are never reached by bare
//!   name. A `CALLS` edge always targets a `Function` or a `pending_symbol`;
//!   any other target is a `REFERENCES` edge.
//! - **Order.** Uses resolve after the walk, calls before references: a name
//!   already called from a symbol is not also referenced by it.

use std::collections::HashSet;

use g_mesh_plugin_sdk::wire::{EdgeKind, NodeKind};
use tree_sitter::Node;

use crate::extractor::decls::Declarer;
use crate::extractor::model::{CallReceiver, PendingCall, PendingReference};
use crate::extractor::scope::{
    bound_scope, collect_declaration_names, collect_pattern_names, declares_binding, function_scope,
    is_binding_position, is_locally_bound, type_parameter_scope, Scope,
};
use crate::extractor::syntax::named_children;

/// The calls and references a walk records, resolved once it is over.
#[derive(Debug, Default)]
pub struct UseState {
    calls: Vec<PendingCall>,
    references: Vec<PendingReference>,
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

    /// A parameter list's types and default values; the binding patterns
    /// themselves are bindings, not uses.
    pub(super) fn visit_parameters(&mut self, node: Node<'t>, scope: &Scope) {
        for parameter in named_children(node) {
            match parameter.kind() {
                "required_parameter" | "optional_parameter" => {
                    self.visit_field(parameter, "type", scope);
                    self.visit_field(parameter, "value", scope);
                }
                "assignment_pattern" => self.visit_field(parameter, "right", scope),
                _ => {}
            }
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

    fn record_call(&mut self, name: &str, receiver: CallReceiver, scope: &Scope) {
        self.uses.calls.push(PendingCall { name: name.to_string(), receiver, scope: scope.clone() });
    }

    /// A call expression: the call it names, or for `require(...)` and
    /// `import(...)` with a foldable argument a computed import, then its
    /// type arguments and arguments.
    ///
    /// A receiver that is neither `this`, `super` nor a bare identifier
    /// (`a.b.c()`, `f().g()`) needs a type to resolve: the receiver is walked
    /// and the property records nothing.
    pub(super) fn handle_call(&mut self, node: Node<'t>, scope: &Scope) {
        if let Some(callee) = node.child_by_field_name("function") {
            match callee.kind() {
                "identifier" => {
                    let name = self.text(callee);
                    // A `require(...)` that is not read as an import is a call
                    // of the name `require`, which a file may declare itself.
                    if name != "require" || !self.record_call_import(node, scope, Some(callee)) {
                        self.record_call(name, CallReceiver::None, scope);
                    }
                }
                "import" => {
                    self.record_call_import(node, scope, None);
                }
                "super" => self.record_call("constructor", CallReceiver::Super, scope),
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
                            self.record_call(self.text(property), receiver, scope);
                        }
                        (Some(object), Some(property)) if object.kind() == "identifier" => {
                            let receiver = CallReceiver::Qualified(self.text(object).to_string());
                            self.record_call(self.text(property), receiver, scope);
                        }
                        (Some(object), _) => self.visit(object, scope),
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
                let receiver = CallReceiver::Qualified(self.text(constructor).to_string());
                self.record_call("constructor", receiver, scope);
            }
            Some(constructor) => self.visit(constructor, scope),
            None => {}
        }
        self.visit_field(node, "type_arguments", scope);
        if let Some(arguments) = node.child_by_field_name("arguments") {
            self.visit_children(arguments, scope);
        }
    }

    /// An identifier outside a binding position is a use of its name.
    pub(super) fn record_reference(&mut self, node: Node<'t>, scope: &Scope) {
        if is_binding_position(node) {
            return;
        }
        self.uses.references.push(PendingReference {
            name: self.text(node).to_string(),
            scope: scope.clone(),
            type_position: node.kind() == "type_identifier",
        });
    }

    /// The `require` callees whose argument did not fold, as calls of
    /// `require`, after every call the walk recorded.
    pub(super) fn record_require_calls(&mut self, callees: Vec<(Node<'t>, Scope)>) {
        for (callee, scope) in callees {
            self.record_call(self.text(callee), CallReceiver::None, &scope);
        }
    }

    // --- resolving uses -------------------------------------------------------

    /// Resolves every recorded call, then every recorded reference.
    pub(super) fn resolve_uses(&mut self) {
        for call in std::mem::take(&mut self.uses.calls) {
            self.resolve_call(&call);
        }
        for reference in std::mem::take(&mut self.uses.references) {
            self.resolve_reference(&reference.name, &reference.scope, reference.type_position);
        }
    }

    fn resolve_call(&mut self, call: &PendingCall) {
        // A locally bound name is not a graph symbol. `this.m()`/`super.m()`
        // name a member, which no local shadows.
        let shadowable = match &call.receiver {
            CallReceiver::None => Some(call.name.as_str()),
            CallReceiver::Qualified(object) => Some(object.as_str()),
            CallReceiver::This | CallReceiver::Super => None,
        };
        if shadowable.is_some_and(|name| is_locally_bound(name, call.scope.locals.as_ref())) {
            return;
        }

        if let Some(target) = self.lookup_call_target(call) {
            match &call.scope.enclosing_caller_id {
                Some(caller) if self.model.node(target).kind == NodeKind::Function => {
                    self.add_call(caller, target);
                }
                _ => self.add_usage(target, &call.scope),
            }
            return;
        }
        match &call.receiver {
            CallReceiver::None => match self.imported_symbol(&call.name) {
                Some(imported) => match &call.scope.enclosing_caller_id {
                    Some(caller) => self.add_call(caller, imported),
                    None => self.add_usage(imported, &call.scope),
                },
                None => self.resolve_reference(&call.name, &call.scope, false),
            },
            // Which member of an imported receiver is the semantic tier's
            // question; the receiver itself is a use.
            CallReceiver::Qualified(object) => self.resolve_reference(object, &call.scope, false),
            CallReceiver::This | CallReceiver::Super => {}
        }
    }

    fn lookup_call_target(&self, call: &PendingCall) -> Option<usize> {
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
            CallReceiver::Qualified(object) => {
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
    fn resolve_reference(&mut self, name: &str, scope: &Scope, type_position: bool) {
        if is_locally_bound(name, scope.locals.as_ref()) {
            return;
        }
        if type_position && is_locally_bound(name, scope.type_parameters.as_ref()) {
            return;
        }
        let target = self.model.lookup_by_name(name, &scope.namespace_prefix, None);
        if let Some(target) = target.or_else(|| self.imported_symbol(name)) {
            self.add_usage(target, scope);
        }
    }

    /// The `CALLS` edge from `caller` to node `target`.
    fn add_call(&mut self, caller: &str, target: usize) {
        let target_id = self.model.node(target).id.clone();
        self.model.add_edge(caller, EdgeKind::Calls, &target_id);
    }

    /// A `REFERENCES` edge from the enclosing symbol, unless it is the
    /// target itself or already calls it.
    fn add_usage(&mut self, target: usize, scope: &Scope) {
        let from = &scope.enclosing_symbol_id;
        let target_id = self.model.node(target).id.clone();
        if *from == target_id || self.model.has_edge(from, EdgeKind::Calls, &target_id) {
            return;
        }
        self.model.add_edge(from, EdgeKind::References, &target_id);
    }

    /// Member `name` of the type `type_qualified_name`, instance or static.
    fn lookup_member(&self, type_qualified_name: &str, name: &str) -> Option<usize> {
        self.model
            .lookup_qualified(&format!("{type_qualified_name}#{name}"))
            .or_else(|| self.model.lookup_qualified(&format!("{type_qualified_name}.{name}")))
    }

    fn lookup_type(&self, name: &str, scope: &Scope) -> Option<usize> {
        self.model.lookup_by_name(name, &scope.namespace_prefix, Some(NodeKind::Type))
    }
}
