//! The declaration pass: every class, interface, type alias, enum,
//! namespace, function, method, function-valued field and module-level
//! variable becomes a node, with `DEFINES` from the file and `EXPORTS` when
//! exported.
//!
//! Rules the walk keeps:
//!
//! - Nothing inside a function body is a node. Locals, nested functions and
//!   classes declared in a body are walked (their parts may still hold
//!   members of an enclosing type's object types) but declare nothing.
//! - A module-level `const f = () => ...` (or function expression) is a
//!   `Function`; any other module-level binding a `Variable`; destructuring
//!   patterns declare nothing.
//! - A method, or a class field whose value is a function, is a member of the
//!   enclosing class or interface: `.` before a static member's name, `#`
//!   before an instance member's. Outside a class or interface (an object
//!   literal) it declares nothing.
//! - Visibility is `public` when the declaration is exported, including by an
//!   `export { name }`, `export default name` or `export = name` anywhere in
//!   the file, which also adds the `EXPORTS` edge.
//! - A member takes its class or interface's visibility, without an
//!   `EXPORTS` edge of its own; a `private` or `#private` member stays
//!   `file`.
//!
//! Imports, re-exports and computed specifiers are [`crate::extractor::imports`]';
//! function bodies, calls and references are [`crate::extractor::bodies`]'.

use g_mesh_plugin_sdk::wire::{NodeKind, Range};
use g_mesh_plugin_sdk::{CharColumns, RelPath};
use tree_sitter::Node;

use crate::extractor::bodies::UseState;
use crate::extractor::imports::{is_whole_module_reexport, ImportState, SpecifierResolver};
use crate::extractor::keys::{qualified_in, MemberSeparator, Qualified, REEXPORT_ALL_NAME};
use crate::extractor::model::{FileModel, NodeParams, PendingSupertype};
use crate::extractor::scope::{block_scope, type_parameter_scope, Scope};
use crate::extractor::syntax::{
    child_of_kind, children, doc_comment_for, function_signature, has_body, has_child_of_kind,
    heritage_name_tokens, heritage_names, is_private_member, method_native_kind, named_children,
    string_literal_value, text,
};

/// Node types a class can be written as.
const CLASS_TYPES: [&str; 3] = ["class_declaration", "abstract_class_declaration", "class"];

/// Every way of writing a function as an expression. In a declaration
/// position (a `const`, a class field, `export default`) one is a `Function`
/// node; anywhere else it is an unnamed function whose parts are walked.
const FUNCTION_VALUE_TYPES: [&str; 3] = ["arrow_function", "function_expression", "generator_function"];

/// Walks one file's tree, declaring into a [`FileModel`].
pub struct Declarer<'a, 's, 't> {
    pub(super) source: &'s str,
    pub(super) columns: &'a CharColumns<'s>,
    pub(super) model: &'a mut FileModel,
    /// The file being walked, as the resolver is asked from.
    pub(super) path: &'a RelPath,
    pub(super) resolver: &'a SpecifierResolver<'a>,
    /// Names an export statement publishes without declaring them there;
    /// settled against the file's declarations once the walk is over.
    pending_exports: Vec<String>,
    pub(super) imports: ImportState<'t>,
    pub(super) uses: UseState<'t>,
}

impl<'a, 's, 't> Declarer<'a, 's, 't> {
    /// `resolver` answers which project file an import specifier written in
    /// `path` names.
    pub fn new(
        source: &'s str,
        columns: &'a CharColumns<'s>,
        path: &'a RelPath,
        resolver: &'a SpecifierResolver<'a>,
        model: &'a mut FileModel,
    ) -> Self {
        Self {
            source,
            columns,
            model,
            path,
            resolver,
            pending_exports: Vec::new(),
            imports: ImportState::default(),
            uses: UseState::default(),
        }
    }

    /// Declares everything in the tree under `root`, then imports the
    /// specifiers `require(...)`/`import(...)` fold to, marks the names
    /// exported after the fact, resolves heritage and what the bodies use,
    /// settles every node's declaration list and records the calls that may
    /// bind an overload.
    ///
    /// Computed specifiers fold first: they read constants declared anywhere
    /// in the file, and their `IMPORTS` edges precede the late `EXPORTS`
    /// edges in insertion order. Uses resolve last, against every
    /// declaration and import of the file.
    pub fn run(mut self, root: Node<'t>) {
        let scope = Scope::module(self.model.file_id());
        self.visit_children(root, &scope);
        let require_calls = self.resolve_call_imports();
        self.record_require_calls(require_calls);
        for name in std::mem::take(&mut self.pending_exports) {
            if let Some(index) = self.model.lookup_by_name(&name, "", None) {
                self.model.mark_exported(index);
            }
        }
        self.resolve_uses();
        self.model.fill_declaration_lists();
        self.record_overload_call_sites();
    }

    // --- helpers ---------------------------------------------------------

    pub(super) fn text(&self, node: Node<'t>) -> &'s str {
        text(node, self.source)
    }

    pub(super) fn range(&self, node: Node<'t>) -> Range {
        let start = node.start_position();
        let end = node.end_position();
        self.columns.range((start.row, start.column), (end.row, end.column))
    }

    /// The parameters of a declaration spanning `at`, qualified by `qualified`.
    fn params(&self, kind: NodeKind, name: &str, qualified: Qualified, at: Node<'t>) -> NodeParams {
        let mut params = NodeParams::new(kind, name, qualified.qualified_name, self.range(at));
        params.qualified_path = Some(qualified.qualified_path);
        params.has_body = has_body(at);
        params
    }

    pub(super) fn visit_children(&mut self, node: Node<'t>, scope: &Scope) {
        for child in named_children(node) {
            self.visit(child, scope);
        }
    }

    pub(super) fn visit_field(&mut self, node: Node<'t>, field: &str, scope: &Scope) {
        if let Some(child) = node.child_by_field_name(field) {
            self.visit(child, scope);
        }
    }

    // --- dispatch --------------------------------------------------------

    pub(super) fn visit(&mut self, node: Node<'t>, scope: &Scope) {
        match node.kind() {
            "comment" => {}
            "import_statement" => self.handle_import(node),
            "export_statement" => self.handle_export(node, scope),
            "call_expression" => self.handle_call(node, scope),
            "new_expression" => self.handle_new(node, scope),
            "member_expression" => self.handle_member_expression(node, scope),
            "class_declaration"
            | "abstract_class_declaration"
            | "class"
            | "interface_declaration"
            | "type_alias_declaration"
            | "enum_declaration"
            | "function_declaration"
            | "generator_function_declaration"
            | "function_signature"
            | "lexical_declaration"
            | "variable_declaration"
            | "internal_module"
            | "module" => self.visit_declaration(node, scope, false, node),
            "method_definition" | "method_signature" | "abstract_method_signature" => {
                self.handle_method(node, scope)
            }
            "public_field_definition" => self.handle_field(node, scope),
            // Interface properties are not symbols; their types may hold
            // object types whose methods are.
            "property_signature" => self.visit_field(node, "type", scope),
            "formal_parameters" => self.visit_parameters(node, scope),
            "arrow_function" | "function_expression" | "generator_function" => {
                self.visit_function_parts(node, scope)
            }
            "statement_block" | "switch_body" => {
                let inner = block_scope(node, self.source, scope);
                self.visit_children(node, &inner);
            }
            "catch_clause" => self.visit_catch_clause(node, scope),
            "for_statement" | "for_in_statement" => self.visit_for_statement(node, scope),
            // Anonymous forms with type parameters of their own.
            "function_type" | "constructor_type" | "call_signature" | "construct_signature" => {
                let inner = type_parameter_scope(node, self.source, scope.clone());
                self.visit_children(node, &inner);
            }
            "identifier" | "type_identifier" | "shorthand_property_identifier" => {
                self.record_reference(node, scope)
            }
            _ => self.visit_children(node, scope),
        }
    }

    fn visit_declaration(&mut self, node: Node<'t>, scope: &Scope, exported: bool, outer: Node<'t>) {
        match node.kind() {
            "class_declaration" | "abstract_class_declaration" | "class" => {
                self.handle_class(node, scope, exported, outer)
            }
            "interface_declaration" => self.handle_interface(node, scope, exported, outer),
            "type_alias_declaration" => self.handle_type_alias(node, scope, exported, outer),
            "enum_declaration" => self.handle_enum(node, scope, exported, outer),
            // `function_signature` is a bodiless overload signature or an
            // ambient declaration: the same symbol its implementation is.
            "function_declaration" | "generator_function_declaration" | "function_signature" => {
                self.handle_function_declaration(node, scope, exported, outer)
            }
            "lexical_declaration" | "variable_declaration" => {
                self.handle_variable_declaration(node, scope, exported, outer)
            }
            "internal_module" | "module" => self.handle_namespace(node, scope, exported, outer),
            _ => self.visit(node, scope),
        }
    }

    // --- exports ---------------------------------------------------------

    /// An export statement: `export <declaration>`, `export default <value>`,
    /// and the local names `export { a, b }` and `export = a` publish. With
    /// `from` it also imports that module, and a clause or `*` records a
    /// re-export placeholder per published name when the module resolved to a
    /// project file; nothing is declared here.
    fn handle_export(&mut self, node: Node<'t>, scope: &Scope) {
        let source = node.child_by_field_name("source");
        let target_path = source.and_then(|source| self.record_import(source));

        if let Some(declaration) = node.child_by_field_name("declaration") {
            self.visit_declaration(declaration, scope, true, node);
            return;
        }
        if let Some(value) = node.child_by_field_name("value") {
            self.handle_default_export_value(value, scope, node);
            return;
        }
        if let Some(clause) = child_of_kind(node, "export_clause") {
            for spec in named_children(clause) {
                if spec.kind() != "export_specifier" {
                    continue;
                }
                let Some(name) = spec.child_by_field_name("name") else { continue };
                let name_text = self.text(name);
                if source.is_none() {
                    self.pending_exports.push(name_text.to_string());
                } else if let Some(target_path) = &target_path {
                    let alias = spec.child_by_field_name("alias");
                    let published = alias.map_or(name_text, |alias| self.text(alias));
                    self.record_reexport(alias.unwrap_or(name), published, target_path, name_text);
                }
            }
            return;
        }
        if is_whole_module_reexport(node) {
            if let Some(target_path) = &target_path {
                self.record_reexport(node, REEXPORT_ALL_NAME, target_path, REEXPORT_ALL_NAME);
            }
            return;
        }
        // `export = foo`.
        if let Some(identifier) = named_children(node).into_iter().find(|child| child.kind() == "identifier")
        {
            self.pending_exports.push(self.text(identifier).to_string());
        }
    }

    fn handle_default_export_value(&mut self, value: Node<'t>, scope: &Scope, outer: Node<'t>) {
        if CLASS_TYPES.contains(&value.kind()) {
            self.handle_class(value, scope, true, outer);
            return;
        }
        if FUNCTION_VALUE_TYPES.contains(&value.kind()) {
            // `export default () => {}`: the export name is all there is.
            let mut params = self.params(
                NodeKind::Function,
                "default",
                qualified_in(&scope.prefix, "default", MemberSeparator::Dot),
                value,
            );
            params.native_kind = Some(value.kind().to_string());
            params.signature = Some(function_signature("default", value, self.source));
            params.doc_comment = doc_comment_for(outer, self.source);
            params.exported = true;
            let index = self.model.declare_symbol(params);
            self.visit_function_body(value, scope, index);
            return;
        }
        if value.kind() == "identifier" {
            self.pending_exports.push(self.text(value).to_string());
            return;
        }
        self.visit(value, scope);
    }

    // --- declarations ----------------------------------------------------

    fn handle_class(&mut self, node: Node<'t>, scope: &Scope, exported: bool, outer: Node<'t>) {
        // `export default class {}` has no name.
        let name = node.child_by_field_name("name").map_or("default", |name| self.text(name));
        let qualified = qualified_in(&scope.prefix, name, MemberSeparator::Dot);
        let body = node.child_by_field_name("body");
        let heritage = child_of_kind(node, "class_heritage");
        let supertype_tokens = heritage.map(heritage_name_tokens).unwrap_or_default();
        let supertype_names =
            heritage.map(|heritage| heritage_names(heritage, self.source)).unwrap_or_default();

        if scope.inside_function {
            let local = type_parameter_scope(
                node,
                self.source,
                Scope { enclosing_type_qname: None, supertype_names, ..scope.clone() },
            );
            if let Some(body) = body {
                self.visit_children(body, &local);
            }
            return;
        }

        let qualified_name = qualified.qualified_name.clone();
        let qualified_path = qualified.qualified_path.clone();
        let mut params = self.params(NodeKind::Type, name, qualified, node);
        params.native_kind = Some(
            if node.kind() == "abstract_class_declaration" { "abstract_class" } else { "class" }.to_string(),
        );
        params.doc_comment = doc_comment_for(outer, self.source);
        params.exported = exported;
        let index = self.model.declare_symbol(params);
        self.record_supertypes(index, &supertype_tokens, scope);

        let member_scope = type_parameter_scope(
            node,
            self.source,
            Scope {
                prefix: qualified_path,
                enclosing_caller_id: None,
                enclosing_symbol_id: self.model.node(index).id.clone(),
                enclosing_type_qname: Some(qualified_name),
                supertype_names,
                ..scope.clone()
            },
        );
        self.visit_field(node, "type_parameters", &member_scope);
        if let Some(heritage) = heritage {
            self.visit_heritage_type_arguments(heritage, &member_scope);
        }
        if let Some(body) = body {
            self.visit_children(body, &member_scope);
        }
    }

    fn handle_interface(&mut self, node: Node<'t>, scope: &Scope, exported: bool, outer: Node<'t>) {
        let Some(name_node) = node.child_by_field_name("name") else { return };
        let name = self.text(name_node);
        let qualified = qualified_in(&scope.prefix, name, MemberSeparator::Dot);
        let body = node.child_by_field_name("body");

        if scope.inside_function {
            if let Some(body) = body {
                let inner = type_parameter_scope(node, self.source, scope.clone());
                self.visit_children(body, &inner);
            }
            return;
        }

        let qualified_name = qualified.qualified_name.clone();
        let qualified_path = qualified.qualified_path.clone();
        let mut params = self.params(NodeKind::Type, name, qualified, node);
        params.native_kind = Some("interface".to_string());
        params.doc_comment = doc_comment_for(outer, self.source);
        params.exported = exported;
        let index = self.model.declare_symbol(params);

        let extends_clause = child_of_kind(node, "extends_type_clause");
        let supertype_tokens = extends_clause.map(heritage_name_tokens).unwrap_or_default();
        let supertype_names =
            extends_clause.map(|clause| heritage_names(clause, self.source)).unwrap_or_default();
        self.record_supertypes(index, &supertype_tokens, scope);
        let member_scope = type_parameter_scope(
            node,
            self.source,
            Scope {
                prefix: qualified_path,
                enclosing_caller_id: None,
                enclosing_symbol_id: self.model.node(index).id.clone(),
                enclosing_type_qname: Some(qualified_name),
                supertype_names,
                ..scope.clone()
            },
        );
        self.visit_field(node, "type_parameters", &member_scope);
        if let Some(clause) = extends_clause {
            self.visit_heritage_type_arguments(clause, &member_scope);
        }
        if let Some(body) = body {
            self.visit_children(body, &member_scope);
        }
    }

    fn handle_type_alias(&mut self, node: Node<'t>, scope: &Scope, exported: bool, outer: Node<'t>) {
        let Some(name_node) = node.child_by_field_name("name") else { return };
        if scope.inside_function {
            let inner = type_parameter_scope(node, self.source, scope.clone());
            self.visit_field(node, "value", &inner);
            return;
        }
        let name = self.text(name_node);
        let mut params =
            self.params(NodeKind::Type, name, qualified_in(&scope.prefix, name, MemberSeparator::Dot), node);
        params.native_kind = Some("type_alias".to_string());
        params.doc_comment = doc_comment_for(outer, self.source);
        params.exported = exported;
        let index = self.model.declare_symbol(params);
        let inner = type_parameter_scope(
            node,
            self.source,
            Scope { enclosing_symbol_id: self.model.node(index).id.clone(), ..scope.clone() },
        );
        self.visit_field(node, "value", &inner);
    }

    /// Enum members are below symbol granularity: only the enum is a node.
    /// Its string members are kept for specifier folding.
    fn handle_enum(&mut self, node: Node<'t>, scope: &Scope, exported: bool, outer: Node<'t>) {
        let Some(name_node) = node.child_by_field_name("name") else { return };
        if scope.inside_function {
            return;
        }
        let name = self.text(name_node);
        let mut params =
            self.params(NodeKind::Type, name, qualified_in(&scope.prefix, name, MemberSeparator::Dot), node);
        params.native_kind = Some("enum".to_string());
        params.doc_comment = doc_comment_for(outer, self.source);
        params.exported = exported;
        let index = self.model.declare_symbol(params);
        let qualified_name = self.model.node(index).qualified_name.clone();
        self.record_enum_member_values(node, &qualified_name);
    }

    /// `namespace N {}` (`namespace`) and `declare module "x" {}`
    /// (`ambient_module`, named by the string's value).
    fn handle_namespace(&mut self, node: Node<'t>, scope: &Scope, exported: bool, outer: Node<'t>) {
        let Some(name_node) = node.child_by_field_name("name") else { return };
        let body = node.child_by_field_name("body");
        let name =
            string_literal_value(name_node, self.source).unwrap_or_else(|| self.text(name_node).to_string());

        if scope.inside_function {
            if let Some(body) = body {
                self.visit_children(body, scope);
            }
            return;
        }

        let qualified = qualified_in(&scope.prefix, &name, MemberSeparator::Dot);
        let qualified_name = qualified.qualified_name.clone();
        let qualified_path = qualified.qualified_path.clone();
        let mut params = self.params(NodeKind::Module, &name, qualified, node);
        params.native_kind =
            Some(if node.kind() == "module" { "ambient_module" } else { "namespace" }.to_string());
        params.doc_comment = doc_comment_for(outer, self.source);
        params.exported = exported;
        let index = self.model.declare_symbol(params);

        let Some(body) = body else { return };
        let inner = Scope {
            prefix: qualified_path,
            namespace_prefix: qualified_name,
            enclosing_symbol_id: self.model.node(index).id.clone(),
            enclosing_type_qname: None,
            supertype_names: Vec::new(),
            ..scope.clone()
        };
        self.visit_children(body, &inner);
    }

    fn handle_function_declaration(
        &mut self,
        node: Node<'t>,
        scope: &Scope,
        exported: bool,
        outer: Node<'t>,
    ) {
        let name_node = node.child_by_field_name("name");
        let Some(name_node) = name_node.filter(|_| !scope.inside_function) else {
            // A nested function declaration is a local.
            self.visit_function_parts(node, scope);
            return;
        };
        let name = self.text(name_node);
        let mut params = self.params(
            NodeKind::Function,
            name,
            qualified_in(&scope.prefix, name, MemberSeparator::Dot),
            node,
        );
        params.native_kind = Some(
            if node.kind() == "generator_function_declaration" { "generator_function" } else { "function" }
                .to_string(),
        );
        params.signature = Some(function_signature(name, node, self.source));
        params.doc_comment = doc_comment_for(outer, self.source);
        params.exported = exported;
        let index = self.model.declare_symbol(params);
        self.visit_function_body(node, scope, index);
    }

    fn handle_method(&mut self, node: Node<'t>, scope: &Scope) {
        let name_node = node.child_by_field_name("name");
        let Some(name_node) =
            name_node.filter(|_| scope.enclosing_type_qname.is_some() && !scope.inside_function)
        else {
            // An object-literal method has no type to be a member of.
            self.visit_function_parts(node, scope);
            return;
        };
        let name = self.text(name_node);
        let is_static = has_child_of_kind(node, "static");
        let separator = if is_static { MemberSeparator::Dot } else { MemberSeparator::Hash };
        let mut params =
            self.params(NodeKind::Function, name, qualified_in(&scope.prefix, name, separator), node);
        params.native_kind = Some(method_native_kind(node, name, is_static).to_string());
        params.signature = Some(function_signature(name, node, self.source));
        params.doc_comment = doc_comment_for(node, self.source);
        params.owner_id = Some(scope.enclosing_symbol_id.clone());
        params.private_member = is_private_member(node, name_node);
        let index = self.model.declare_symbol(params);
        self.visit_function_body(node, scope, index);
    }

    /// Only a function-valued class field (`fire = () => ...`) is a node;
    /// data properties are below symbol granularity.
    fn handle_field(&mut self, node: Node<'t>, scope: &Scope) {
        let name_node = node.child_by_field_name("name");
        let value = node.child_by_field_name("value");
        let member = match (name_node, value) {
            (Some(name_node), Some(value))
                if FUNCTION_VALUE_TYPES.contains(&value.kind())
                    && scope.enclosing_type_qname.is_some()
                    && !scope.inside_function =>
            {
                Some((name_node, value))
            }
            _ => None,
        };
        let Some((name_node, value)) = member else {
            self.visit_field(node, "type", scope);
            if let Some(value) = value {
                self.visit(value, scope);
            }
            return;
        };
        let name = self.text(name_node);
        let is_static = has_child_of_kind(node, "static");
        let separator = if is_static { MemberSeparator::Dot } else { MemberSeparator::Hash };
        let mut params =
            self.params(NodeKind::Function, name, qualified_in(&scope.prefix, name, separator), node);
        params.native_kind = Some(value.kind().to_string());
        params.signature = Some(function_signature(name, value, self.source));
        params.doc_comment = doc_comment_for(node, self.source);
        params.owner_id = Some(scope.enclosing_symbol_id.clone());
        params.private_member = is_private_member(node, name_node);
        let index = self.model.declare_symbol(params);
        self.visit_function_body(value, scope, index);
    }

    fn handle_variable_declaration(
        &mut self,
        node: Node<'t>,
        scope: &Scope,
        exported: bool,
        outer: Node<'t>,
    ) {
        let keyword = children(node)
            .into_iter()
            .find(|child| matches!(child.kind(), "const" | "let" | "var"))
            .map(|k| k.kind());

        for declarator in named_children(node) {
            if declarator.kind() != "variable_declarator" {
                continue;
            }
            let name_node = declarator.child_by_field_name("name");
            let value = declarator.child_by_field_name("value");

            // Locals and destructuring patterns declare nothing; a pattern's
            // defaults and computed keys belong to the enclosing scope.
            let Some(name_node) =
                name_node.filter(|name| !scope.inside_function && name.kind() == "identifier")
            else {
                self.visit_field(declarator, "type", scope);
                if let Some(value) = value {
                    self.visit(value, scope);
                }
                if let Some(pattern) = name_node {
                    self.visit_binding_pattern(pattern, scope);
                }
                continue;
            };
            let name = self.text(name_node);
            let qualified = qualified_in(&scope.prefix, name, MemberSeparator::Dot);

            if let Some(value) = value.filter(|value| FUNCTION_VALUE_TYPES.contains(&value.kind())) {
                let mut params = self.params(NodeKind::Function, name, qualified, declarator);
                params.native_kind = Some(value.kind().to_string());
                params.signature = Some(function_signature(name, value, self.source));
                params.doc_comment = doc_comment_for(outer, self.source);
                params.exported = exported;
                let index = self.model.declare_symbol(params);
                self.visit_function_body(value, scope, index);
                continue;
            }

            let mut params = self.params(NodeKind::Variable, name, qualified, declarator);
            params.native_kind = Some(keyword.unwrap_or("var").to_string());
            params.doc_comment = doc_comment_for(outer, self.source);
            params.exported = exported;
            let index = self.model.declare_symbol(params);
            if let Some(value) = value.filter(|_| keyword == Some("const")) {
                self.record_constant_initializer(index, name, value, scope);
            }
            let value_scope =
                Scope { enclosing_symbol_id: self.model.node(index).id.clone(), ..scope.clone() };
            self.visit_field(declarator, "type", &value_scope);
            if let Some(value) = value {
                self.visit(value, &value_scope);
            }
        }
    }

    // --- heritage ----------------------------------------------------------

    /// Each heritage name of type node `index`, a `SUPERTYPE_OF` edge once
    /// every declaration and import is known.
    fn record_supertypes(&mut self, index: usize, tokens: &[Node<'t>], scope: &Scope) {
        let from_id = self.model.node(index).id.clone();
        for token in tokens {
            self.uses.supertypes.push(PendingSupertype {
                from_id: from_id.clone(),
                name: self.text(*token).to_string(),
                scope: scope.clone(),
                at: *token,
            });
        }
    }

    /// The type arguments of a heritage clause (`extends Box<{ m(): void }>`),
    /// walked in the member scope of the class or interface.
    fn visit_heritage_type_arguments(&mut self, clause: Node<'t>, scope: &Scope) {
        for child in named_children(clause) {
            match child.kind() {
                "extends_clause" | "implements_clause" => self.visit_heritage_type_arguments(child, scope),
                "type_arguments" => self.visit_children(child, scope),
                "generic_type" => {
                    if let Some(arguments) = child.child_by_field_name("type_arguments") {
                        self.visit_children(arguments, scope);
                    }
                }
                _ => {}
            }
        }
    }
}
