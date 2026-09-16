//! The body pass: every edge, every placeholder that is not a `use`, and
//! every open site.
//!
//! # The one question, asked in four shapes
//!
//! Each use site reduces to "what does this path name, from here":
//!
//! | Written | Addressed as |
//! |---|---|
//! | `f()`, `CONST` | this module's own declaration, this module's `use` items, or nothing |
//! | `a::b::f()` | a `name` key in the container `a::b` resolves to |
//! | `T::f()`, `Self::f()`, `self.f()` | a `qualifiedName` key - `…::T::f` - in the container `T` lives in |
//! | `x.m()` | nothing: an open site for the semantic tier |
//!
//! The split between the second and third rows is the naming convention
//! ([`looks_like_type`](super::syntax::looks_like_type)), and the reason they
//! are addressed differently is ambiguity. A module holding
//! `impl Reader { fn new() }` and `impl Writer { fn new() }` - which is most
//! modules - offers two declarations *named* `new`, so a `name`-keyed
//! `Reader::new()` would find two candidates and core would rightly refuse
//! both. `qualifiedName` is exact, and `Reader::new`'s qualified name is
//! exactly what this plugin gave it.
//!
//! The reverse trade is why a *module*-qualified call keeps a `name` key:
//! only `name` keys walk re-export chains (`graph::symbol_links`: "a
//! `qualifiedName` names a declaration, never a pass-through"), and
//! `a::b::f()` where `a::b` re-exports `f` from somewhere else is ordinary
//! Rust. A module-qualified name is ambiguous only when one module holds a
//! free `fn f` *and* a method `f`, which is rare and fails to a missing edge.
//!
//! # Decision 7: what becomes an open site
//!
//! An open site is a question this tier cannot answer and
//! `rust-analyzer` (GM-290) can. It is recorded for:
//!
//!  - **`x.m()`** - a receiver call, the case open sites exist for.
//!  - **A call whose path does not resolve**: a bare name that is neither
//!    declared here nor imported (it came through a glob import, or the
//!    prelude), an associated function on a generic parameter (`T::new()`),
//!    `Self::` outside any impl.
//!  - **`impl Trait for T` where `T` is not declared in this file.** The
//!    `SUPERTYPE_OF` edge has to start at `T`'s node, and an edge may not
//!    leave its file - but a *semantic* answer may (the conformance kit
//!    exempts `semanticPass` diffs from the per-file rules), so the question
//!    is recorded at the type's position for rust-analyzer's
//!    `textDocument/definition` to answer.
//!
//! It is deliberately **not** recorded for an unresolved *type* reference.
//! `Vec`, `String`, `Option`, `Result` and every other name from another
//! crate would become one, which would swamp a bridge that has a per-pass
//! site budget with questions whose answers are not in the index anyway. A
//! type this project declares is reachable through a `use`, which resolves.

use g_mesh_plugin_sdk::wire::{EdgeKind, NodeKind, PlaceholderTarget, TargetKey};
use g_mesh_plugin_sdk::{OpenSite, OpenSiteKind, PlaceholderKind};
use tree_sitter::Node;

use crate::extractor::decls::{impl_block, member_tail, trait_block, BlockCtx, Family};
use crate::extractor::emit::{container_target, Emitter};
use crate::extractor::keys::{qualified_in, resolve_module_path, visibility, ModuleCtx, PathTarget};
use crate::extractor::model::{FileModel, Import};
use crate::extractor::scope::Scopes;
use crate::extractor::syntax::{flatten_path, item_name, looks_like_type, path_tail, text, Seg};
use crate::project::ProjectContext;

/// What a use site turned out to name.
#[derive(Debug, Clone)]
enum Bound {
    /// A declaration of this same file: a direct, `resolved: true` edge.
    Here(String),
    /// Something another file may declare: a placeholder at this address.
    There { target: PlaceholderTarget, name: String },
    /// Nothing this tier can say - see the module doc, Decision 7.
    Open,
    /// Nothing at all: a local binding, a generic parameter, an external
    /// crate's name. Not a question, so not an open site either.
    Nothing,
}

/// Walks a file's bodies against what [`Declarer`](super::decls::Declarer)
/// found.
pub(crate) struct Bodies<'a, 's> {
    pub(crate) project: &'a ProjectContext,
    pub(crate) source: &'s str,
    pub(crate) model: &'a FileModel,
    pub(crate) emitter: &'a mut Emitter<'s>,
    pub(crate) scopes: Scopes,
}

impl Bodies<'_, '_> {
    /// Walks one node, with everything that decides what its names mean: the
    /// module it is in, the `impl`/`trait` block if any, and the declaration
    /// an edge found here would start at.
    pub(crate) fn visit(&mut self, node: Node, module: &ModuleCtx, block: Option<&BlockCtx>, from: &str) {
        match node.kind() {
            // Handled by the declaration pass, or deliberately not read.
            "use_declaration"
            | "extern_crate_declaration"
            | "line_comment"
            | "block_comment"
            | "attribute_item"
            | "inner_attribute_item" => {}
            // Decision 5: a `macro_rules!` body is token trees, not code. It
            // is not parsed, and the items it expands to are a documented
            // structural gap.
            "macro_definition" => {}
            "macro_invocation" => self.macro_invocation(node, module, from),
            "mod_item" => self.mod_item(node, module, from),
            "impl_item" => self.impl_item(node, module, from),
            "trait_item" => self.trait_item(node, module, from),
            "function_item" | "function_signature_item" => self.function(node, module, block, from),
            "struct_item" | "enum_item" | "union_item" | "type_item" | "associated_type" => {
                self.type_declaration(node, module, block, from)
            }
            "const_item" | "static_item" => {
                let id = self.declaration_id(node, module, block).unwrap_or_else(|| from.to_string());
                // Skipping `name` for the same reason `function` does: it is
                // an `identifier`, and asking what it means here would ask
                // about the thing being declared - a node referencing itself.
                self.visit_children_except(node, &[node.child_by_field_name("name")], module, block, &id);
            }
            // An enum variant is not a node (see `decls`), and its name is an
            // `identifier` that would otherwise read as a use of whatever the
            // module declares under that name.
            "enum_variant" => {
                self.visit_children_except(node, &[node.child_by_field_name("name")], module, block, from);
            }
            // A lifetime and a loop label are `identifier`s in namespaces of
            // their own. Walking them is how `fn f<'a>(…)` in a crate with a
            // `mod a` produces a reference to that module - a wrong edge from
            // a name that is not even in the same namespace.
            "lifetime" | "label" => {}
            "call_expression" => self.call(node, module, block, from),
            "let_declaration" => self.let_declaration(node, module, block, from),
            "closure_expression" => self.closure(node, module, block, from),
            "for_expression" => self.for_expression(node, module, block, from),
            "if_expression" | "while_expression" => self.conditional(node, module, block, from),
            "match_arm" => self.match_arm(node, module, block, from),
            "block" => {
                self.scopes.push();
                self.visit_children(node, module, block, from);
                self.scopes.pop();
            }
            // `p.x` - the field name is not a symbol this index carries, so
            // only the receiver is walked.
            "field_expression" => {
                if let Some(value) = node.child_by_field_name("value") {
                    self.visit(value, module, block, from);
                }
            }
            "identifier" => self.bare_use(node, module, block, from, None, EdgeKind::References),
            "scoped_identifier" => self.path_use(node, module, block, from, None, EdgeKind::References),
            "type_identifier" => {
                self.bare_use(node, module, block, from, Some(NodeKind::Type), EdgeKind::References)
            }
            "scoped_type_identifier" => {
                self.path_use(node, module, block, from, Some(NodeKind::Type), EdgeKind::References)
            }
            _ => self.visit_children(node, module, block, from),
        }
    }

    pub(crate) fn visit_children(
        &mut self,
        node: Node,
        module: &ModuleCtx,
        block: Option<&BlockCtx>,
        from: &str,
    ) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.visit(child, module, block, from);
        }
    }

    /// Every named child except the ones a caller has already handled - so
    /// that an `impl`'s trait clause, having produced a `SUPERTYPE_OF` edge,
    /// does not also produce a `REFERENCES` one saying the same thing twice.
    fn visit_children_except(
        &mut self,
        node: Node,
        skip: &[Option<Node>],
        module: &ModuleCtx,
        block: Option<&BlockCtx>,
        from: &str,
    ) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if skip.contains(&Some(child)) {
                continue;
            }
            self.visit(child, module, block, from);
        }
    }

    /// The id the declaration pass gave this item, so an edge found inside it
    /// starts at the right node.
    fn declaration_id(&self, item: Node, module: &ModuleCtx, block: Option<&BlockCtx>) -> Option<String> {
        let name = item_name(item, self.source)?;
        let tail = member_tail(block, name);
        self.model.lookup_tail(&module.key, &tail).map(|decl| decl.id.clone())
    }

    // --- items ----------------------------------------------------------------

    fn mod_item(&mut self, item: Node, module: &ModuleCtx, from: &str) {
        let Some(name) = item_name(item, self.source) else { return };
        let Some(body) = item.child_by_field_name("body") else { return };
        let child = module.child(name);
        let id = self
            .model
            .lookup_name(&module.key, name, Some(NodeKind::Module))
            .map(|decl| decl.id.clone())
            .unwrap_or_else(|| from.to_string());
        self.visit_children(body, &child, None, &id);
    }

    fn impl_item(&mut self, item: Node, module: &ModuleCtx, from: &str) {
        let block = impl_block(item, self.source);
        let parameters = item.child_by_field_name("type_parameters");
        let trait_clause = item.child_by_field_name("trait");
        let self_type = item.child_by_field_name("type");

        self.scopes.push();
        if let Some(parameters) = parameters {
            self.scopes.bind_type_parameters(parameters, self.source);
        }
        if let Some(self_type) = self_type {
            match trait_clause {
                // `impl Tr for T` is the edge `find_implementations` walks,
                // subtype -> supertype.
                Some(clause) => self.supertype_edge(self_type, clause, module, Some(&block), from),
                // An inherent `impl T` is a *use* of `T`, worth a reference
                // so that `find_references` on a type shows where it is
                // implemented.
                None => {
                    self.visit(self_type, module, Some(&block), from);
                }
            }
        }
        self.visit_children_except(
            item,
            &[trait_clause, self_type, item.child_by_field_name("body")],
            module,
            Some(&block),
            from,
        );
        if let Some(body) = item.child_by_field_name("body") {
            self.visit_children(body, module, Some(&block), from);
        }
        self.scopes.pop();
    }

    fn trait_item(&mut self, item: Node, module: &ModuleCtx, from: &str) {
        let own = visibility(item, module, self.source);
        let block = trait_block(item, self.source, &own);
        let id = self.declaration_id(item, module, None).unwrap_or_else(|| from.to_string());
        let bounds = item.child_by_field_name("bounds");

        self.scopes.push();
        if let Some(parameters) = item.child_by_field_name("type_parameters") {
            self.scopes.bind_type_parameters(parameters, self.source);
        }
        // `trait Sub: Super` is the same relation as `impl Super for Sub`:
        // every implementor of `Sub` is one of `Super`.
        if let Some(bounds) = bounds {
            let mut cursor = bounds.walk();
            for bound in bounds.named_children(&mut cursor) {
                if bound.kind() == "lifetime" {
                    continue;
                }
                self.supertype_to(&id, bound, module, Some(&block));
            }
        }
        self.visit_children_except(
            item,
            &[bounds, item.child_by_field_name("body"), item.child_by_field_name("name")],
            module,
            Some(&block),
            &id,
        );
        if let Some(body) = item.child_by_field_name("body") {
            self.visit_children(body, module, Some(&block), &id);
        }
        self.scopes.pop();
    }

    fn function(&mut self, item: Node, module: &ModuleCtx, block: Option<&BlockCtx>, from: &str) {
        let id = self.declaration_id(item, module, block).unwrap_or_else(|| from.to_string());
        self.scopes.push();
        if let Some(parameters) = item.child_by_field_name("type_parameters") {
            self.scopes.bind_type_parameters(parameters, self.source);
        }
        if let Some(parameters) = item.child_by_field_name("parameters") {
            self.scopes.bind_parameters(parameters, self.source);
        }
        // The name is an `identifier`, and walking it would ask what `f`
        // means at a point where `f` is the thing being declared.
        self.visit_children_except(item, &[item.child_by_field_name("name")], module, block, &id);
        self.scopes.pop();
    }

    fn type_declaration(&mut self, item: Node, module: &ModuleCtx, block: Option<&BlockCtx>, from: &str) {
        let id = self.declaration_id(item, module, block).unwrap_or_else(|| from.to_string());
        self.scopes.push();
        if let Some(parameters) = item.child_by_field_name("type_parameters") {
            self.scopes.bind_type_parameters(parameters, self.source);
        }
        self.visit_children_except(item, &[item.child_by_field_name("name")], module, block, &id);
        self.scopes.pop();
    }

    // --- expressions ----------------------------------------------------------

    fn let_declaration(&mut self, node: Node, module: &ModuleCtx, block: Option<&BlockCtx>, from: &str) {
        // Before the pattern is bound: `let x = x;` reads the *outer* `x`,
        // and a `let ... else` block runs where the binding does not exist.
        for field in ["type", "value", "alternative"] {
            if let Some(child) = node.child_by_field_name(field) {
                self.visit(child, module, block, from);
            }
        }
        if let Some(pattern) = node.child_by_field_name("pattern") {
            self.scopes.bind_pattern(pattern, self.source);
        }
    }

    fn closure(&mut self, node: Node, module: &ModuleCtx, block: Option<&BlockCtx>, from: &str) {
        self.scopes.push();
        if let Some(parameters) = node.child_by_field_name("parameters") {
            self.scopes.bind_parameters(parameters, self.source);
        }
        self.visit_children_except(node, &[node.child_by_field_name("parameters")], module, block, from);
        self.scopes.pop();
    }

    fn for_expression(&mut self, node: Node, module: &ModuleCtx, block: Option<&BlockCtx>, from: &str) {
        if let Some(value) = node.child_by_field_name("value") {
            self.visit(value, module, block, from);
        }
        self.scopes.push();
        if let Some(pattern) = node.child_by_field_name("pattern") {
            self.scopes.bind_pattern(pattern, self.source);
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.visit(body, module, block, from);
        }
        self.scopes.pop();
    }

    /// `if let`/`while let`: the pattern binds in the body, and the value it
    /// matches is evaluated before it does.
    fn conditional(&mut self, node: Node, module: &ModuleCtx, block: Option<&BlockCtx>, from: &str) {
        self.scopes.push();
        let condition = node.child_by_field_name("condition");
        if let Some(condition) = condition {
            if condition.kind() == "let_condition" || condition.kind() == "let_chain" {
                self.bind_let_condition(condition, module, block, from);
            } else {
                self.visit(condition, module, block, from);
            }
        }
        self.visit_children_except(node, &[condition], module, block, from);
        self.scopes.pop();
    }

    fn bind_let_condition(
        &mut self,
        condition: Node,
        module: &ModuleCtx,
        block: Option<&BlockCtx>,
        from: &str,
    ) {
        if condition.kind() == "let_chain" {
            let mut cursor = condition.walk();
            for child in condition.named_children(&mut cursor) {
                self.bind_let_condition(child, module, block, from);
            }
            return;
        }
        if condition.kind() != "let_condition" {
            self.visit(condition, module, block, from);
            return;
        }
        if let Some(value) = condition.child_by_field_name("value") {
            self.visit(value, module, block, from);
        }
        if let Some(pattern) = condition.child_by_field_name("pattern") {
            self.scopes.bind_pattern(pattern, self.source);
        }
    }

    fn match_arm(&mut self, node: Node, module: &ModuleCtx, block: Option<&BlockCtx>, from: &str) {
        self.scopes.push();
        let pattern = node.child_by_field_name("pattern");
        let mut guard = None;
        if let Some(pattern) = pattern {
            guard = pattern.child_by_field_name("condition");
            let mut cursor = pattern.walk();
            for child in pattern.named_children(&mut cursor) {
                if Some(child) == guard {
                    continue;
                }
                self.scopes.bind_pattern(child, self.source);
            }
        }
        // The guard is ordinary code, and it sees the arm's bindings.
        if let Some(guard) = guard {
            self.visit(guard, module, block, from);
        }
        if let Some(value) = node.child_by_field_name("value") {
            self.visit(value, module, block, from);
        }
        self.scopes.pop();
    }

    fn call(&mut self, node: Node, module: &ModuleCtx, block: Option<&BlockCtx>, from: &str) {
        let Some(function) = node.child_by_field_name("function") else {
            self.visit_children(node, module, block, from);
            return;
        };
        // `f::<T>()` - the turbofish wraps the path it applies to.
        let target = if function.kind() == "generic_function" {
            function.child_by_field_name("function").unwrap_or(function)
        } else {
            function
        };

        match target.kind() {
            "field_expression" => self.receiver_call(target, module, block, from),
            "identifier" => {
                self.bare_use(target, module, block, from, Some(NodeKind::Function), EdgeKind::Calls)
            }
            "scoped_identifier" | "scoped_type_identifier" | "generic_type" => {
                self.path_use(target, module, block, from, Some(NodeKind::Function), EdgeKind::Calls)
            }
            // Calling something that is not a path at all - `(f)()`,
            // `map[&k]()`. The callee is an expression; walking it is the
            // honest answer.
            _ => self.visit(target, module, block, from),
        }
        if let Some(arguments) = node.child_by_field_name("arguments") {
            self.visit(arguments, module, block, from);
        }
    }

    /// `x.m()` - and its one resolvable special case, `self.m()` inside an
    /// `impl`, which is the impl type's own method (the design doc's "like TS
    /// `this`").
    fn receiver_call(&mut self, target: Node, module: &ModuleCtx, block: Option<&BlockCtx>, from: &str) {
        let Some(field) = target.child_by_field_name("field") else { return };
        let Some(value) = target.child_by_field_name("value") else { return };
        let name = text(field, self.source);
        if value.kind() == "self" {
            if let Some(block) = block {
                let bound = self.self_member(block, name, module);
                self.emit(bound, EdgeKind::Calls, from, field, module, OpenSiteKind::Reference);
                return;
            }
        }
        self.visit(value, module, block, from);
        self.open_site(from, field, name, module, OpenSiteKind::ReceiverCall, EdgeKind::Calls);
    }

    fn macro_invocation(&mut self, node: Node, module: &ModuleCtx, from: &str) {
        let Some(name) = node.child_by_field_name("macro") else { return };
        // A macro invocation's arguments are a token tree, not an expression:
        // the calls written inside it have no `call_expression` node to find,
        // which is the honest half of the documented macro gap.
        match name.kind() {
            "identifier" => {
                self.bare_use(name, module, None, from, Some(NodeKind::Function), EdgeKind::Calls)
            }
            "scoped_identifier" => {
                self.path_use(name, module, None, from, Some(NodeKind::Function), EdgeKind::Calls)
            }
            _ => {}
        }
    }

    // --- name resolution ------------------------------------------------------

    /// A single-segment name used as a value or a type.
    fn bare_use(
        &mut self,
        node: Node,
        module: &ModuleCtx,
        block: Option<&BlockCtx>,
        from: &str,
        want: Option<NodeKind>,
        kind: EdgeKind,
    ) {
        let name = text(node, self.source);
        let bound = self.resolve_bare(name, module, block, want);
        self.emit(bound, kind, from, node, module, OpenSiteKind::Reference);
    }

    fn resolve_bare(
        &self,
        name: &str,
        module: &ModuleCtx,
        block: Option<&BlockCtx>,
        want: Option<NodeKind>,
    ) -> Bound {
        if self.scopes.binds(name) {
            return Bound::Nothing;
        }
        // `Self` in type position is the impl's own type; as a path root it
        // is handled by `resolve_type_qualified`.
        if name == "Self" {
            return match (block, want) {
                (Some(block), Some(NodeKind::Type)) => {
                    match self.model.lookup_name(&module.key, &block.self_type, Some(NodeKind::Type)) {
                        Some(decl) => Bound::Here(decl.id.clone()),
                        None => Bound::Nothing,
                    }
                }
                _ => Bound::Nothing,
            };
        }
        if let Some(decl) = self.model.lookup_name(&module.key, name, want) {
            return Bound::Here(decl.id.clone());
        }
        match self.model.lookup_import(&module.key, name) {
            Some(Import::Item { container, name }) => Bound::There {
                target: container_target(container, TargetKey::Name(name.clone()), &module.key),
                name: name.clone(),
            },
            Some(Import::External) => Bound::Nothing,
            // A type nothing here declares or imports is almost always from
            // another crate; a *call* to such a name came through a glob
            // import or the prelude, and only the semantic tier knows which.
            None => {
                if want == Some(NodeKind::Function) {
                    Bound::Open
                } else {
                    Bound::Nothing
                }
            }
        }
    }

    /// A multi-segment path used as a value or a type.
    fn path_use(
        &mut self,
        node: Node,
        module: &ModuleCtx,
        block: Option<&BlockCtx>,
        from: &str,
        want: Option<NodeKind>,
        kind: EdgeKind,
    ) {
        let Some(segments) = flatten_path(node, self.source) else {
            return;
        };
        let bound = self.resolve_path(&segments, module, block, want);
        self.emit(bound, kind, from, node, module, OpenSiteKind::Reference);
    }

    fn resolve_path(
        &self,
        segments: &[Seg<'_>],
        module: &ModuleCtx,
        block: Option<&BlockCtx>,
        want: Option<NodeKind>,
    ) -> Bound {
        let Some(tail) = path_tail(segments) else { return Bound::Nothing };
        if segments.len() == 1 {
            return self.resolve_bare(tail, module, block, want);
        }
        let qualifier = &segments[segments.len() - 2];
        if let Some(qualifier) = qualifier.name() {
            if qualifier == "Self" || looks_like_type(qualifier) {
                return self.resolve_type_qualified(
                    &segments[..segments.len() - 2],
                    qualifier,
                    tail,
                    module,
                    block,
                );
            }
        }
        match resolve_module_path(&segments[..segments.len() - 1], module, self.model, self.project) {
            PathTarget::Container(container) => {
                if let Some(decl) = self.model.lookup_name(&container, tail, want) {
                    return Bound::Here(decl.id.clone());
                }
                Bound::There {
                    target: container_target(&container, TargetKey::Name(tail.to_string()), &module.key),
                    name: tail.to_string(),
                }
            }
            PathTarget::ExternalCrate(_) => Bound::Nothing,
            PathTarget::Unresolved => Bound::Open,
        }
    }

    /// `[prefix::]T::member`, where `T` is a type: addressed by
    /// `qualifiedName`, because a container holds one `T::member` and can
    /// hold many things merely *named* `member`.
    fn resolve_type_qualified(
        &self,
        prefix: &[Seg<'_>],
        type_name: &str,
        member: &str,
        module: &ModuleCtx,
        block: Option<&BlockCtx>,
    ) -> Bound {
        if type_name == "Self" {
            return match block {
                Some(block) if prefix.is_empty() => self.self_member(block, member, module),
                _ => Bound::Open,
            };
        }

        let (container, type_name) = if prefix.is_empty() {
            if self.scopes.binds(type_name) {
                // An associated function on a generic parameter - `T::new()`.
                // Which type that is at each call site is exactly what a
                // structural tier cannot know.
                return Bound::Open;
            }
            match self.model.lookup_import(&module.key, type_name) {
                Some(Import::Item { container, name }) => (container.clone(), name.clone()),
                Some(Import::External) => return Bound::Nothing,
                // Declared here, or reached through a glob import: the own
                // module is the only address worth trying, and a wrong guess
                // simply finds nothing.
                None => (module.key.clone(), type_name.to_string()),
            }
        } else {
            match resolve_module_path(prefix, module, self.model, self.project) {
                PathTarget::Container(container) => (container, type_name.to_string()),
                PathTarget::ExternalCrate(_) => return Bound::Nothing,
                PathTarget::Unresolved => return Bound::Open,
            }
        };

        self.member_of(&container, &type_name, member, module)
    }

    /// `Self::m` / `self.m` inside a block: the trait impl's own member
    /// first, then the type's inherent one.
    ///
    /// The order is Rust's, near enough: inside `impl Tr for T`, `self.m()`
    /// most often means the very method the block is implementing or a
    /// sibling of it. Where it means an inherent `T::m` instead, the second
    /// rung finds it. Where the two genuinely disagree - an inherent method
    /// shadowing a trait method of the same name - only name resolution
    /// settles it, and that is the semantic tier's.
    fn self_member(&self, block: &BlockCtx, member: &str, module: &ModuleCtx) -> Bound {
        if let Some(prefix) = &block.trait_prefix {
            if let Some(decl) = self.model.lookup_tail(&module.key, &format!("{prefix}::{member}")) {
                return Bound::Here(decl.id.clone());
            }
        }
        if block.family == Family::TraitDecl {
            // Inside a trait's own default method, `self.m()` is one of the
            // trait's methods - `Tr::m`, which is this block's prefix.
            if let Some(decl) = self.model.lookup_tail(&module.key, &format!("{}::{member}", block.prefix)) {
                return Bound::Here(decl.id.clone());
            }
        }
        self.member_of(&module.key, &block.self_type, member, module)
    }

    /// The address of `T::member` in `container`, as a same-file declaration
    /// when this file makes it and as a `qualifiedName` placeholder
    /// otherwise.
    fn member_of(&self, container: &str, type_name: &str, member: &str, module: &ModuleCtx) -> Bound {
        let tail = format!("{type_name}::{member}");
        if let Some(decl) = self.model.lookup_tail(container, &tail) {
            return Bound::Here(decl.id.clone());
        }
        Bound::There {
            target: container_target(
                container,
                TargetKey::QualifiedName(qualified_in(container, &tail)),
                &module.key,
            ),
            name: member.to_string(),
        }
    }

    // --- emission -------------------------------------------------------------

    fn emit(
        &mut self,
        bound: Bound,
        kind: EdgeKind,
        from: &str,
        at: Node,
        module: &ModuleCtx,
        open: OpenSiteKind,
    ) {
        match bound {
            Bound::Here(to) => self.emitter.resolved_edge(kind, from, &to),
            Bound::There { target, name } => {
                let range = self.emitter.positions().range(at);
                let placeholder =
                    self.emitter.placeholder(PlaceholderKind::PendingSymbol, &name, target, range);
                self.emitter.placeholder_edge(kind, from, &placeholder);
            }
            Bound::Open => {
                let name = text(at, self.source).to_string();
                self.open_site(from, at, &name, module, open, kind);
            }
            Bound::Nothing => {}
        }
    }

    fn open_site(
        &mut self,
        from: &str,
        at: Node,
        name: &str,
        module: &ModuleCtx,
        kind: OpenSiteKind,
        edge_kind: EdgeKind,
    ) {
        let position = self.emitter.positions().at(at.start_position());
        self.emitter.open_site(OpenSite {
            from_id: from.to_string(),
            position,
            name: name.to_string(),
            kind,
            edge_kind,
            from_container: Some(module.key.clone()),
            // Nothing this tier emitted is being replaced: every open site
            // here is a site it wrote *no* edge for (see the module doc,
            // Decision 7), so a semantic answer has nothing to contradict.
            // The field exists for the other shape - a structural edge
            // written on a guess - which this extractor does not produce.
            replaces: None,
        });
    }

    /// `impl Tr for T` - `SUPERTYPE_OF` from `T` to `Tr`, the direction
    /// `find_implementations` walks.
    fn supertype_edge(
        &mut self,
        self_type: Node,
        trait_clause: Node,
        module: &ModuleCtx,
        block: Option<&BlockCtx>,
        from: &str,
    ) {
        let subtype = flatten_path(self_type, self.source)
            .filter(|segments| segments.len() == 1)
            .as_deref()
            .and_then(path_tail)
            .and_then(|name| self.model.lookup_name(&module.key, name, Some(NodeKind::Type)))
            .map(|decl| decl.id.clone());
        let Some(subtype) = subtype else {
            // The implemented type is not declared in this file, so no edge
            // of this file may start at it - see the module doc, Decision 7.
            let name = flatten_path(self_type, self.source)
                .as_deref()
                .and_then(path_tail)
                .unwrap_or_else(|| text(self_type, self.source))
                .to_string();
            self.open_site(
                from,
                self_type,
                &name,
                module,
                OpenSiteKind::Implementation,
                EdgeKind::SupertypeOf,
            );
            return;
        };
        self.supertype_to(&subtype, trait_clause, module, block);
    }

    fn supertype_to(&mut self, subtype: &str, supertype: Node, module: &ModuleCtx, block: Option<&BlockCtx>) {
        let Some(segments) = flatten_path(supertype, self.source) else { return };
        let bound = self.resolve_path(&segments, module, block, Some(NodeKind::Type));
        let subtype = subtype.to_string();
        self.emit(bound, EdgeKind::SupertypeOf, &subtype, supertype, module, OpenSiteKind::Implementation);
    }
}
