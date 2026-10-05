//! The lexical context threaded through the walk.

use std::collections::HashSet;
use std::rc::Rc;

use g_mesh_plugin_sdk::wire::PathSegment;
use tree_sitter::Node;

use crate::extractor::syntax::{children, named_children, text};

/// One link of a scope chain: the names a function, block or declaration
/// binds, and the link enclosing it.
#[derive(Debug)]
pub struct LocalBindings {
    pub names: HashSet<String>,
    pub parent: Option<Rc<LocalBindings>>,
}

/// Whether `name` is bound anywhere along `bindings`' chain.
pub fn is_locally_bound(name: &str, bindings: Option<&Rc<LocalBindings>>) -> bool {
    let mut link = bindings;
    while let Some(scope) = link {
        if scope.names.contains(name) {
            return true;
        }
        link = scope.parent.as_ref();
    }
    false
}

/// Where the walk is.
#[derive(Debug, Clone)]
pub struct Scope {
    /// The path a declaration here is qualified under.
    pub prefix: Vec<PathSegment>,
    /// The part of `prefix` an unqualified name can resolve against: the
    /// enclosing namespaces, never a class.
    pub namespace_prefix: String,
    /// Names bound by the enclosing functions and blocks. None of them is a
    /// graph symbol.
    pub locals: Option<Rc<LocalBindings>>,
    /// Type parameters of the enclosing declarations, which bind in the type
    /// namespace only.
    pub type_parameters: Option<Rc<LocalBindings>>,
    /// The `from` of a call written here: the nearest enclosing function
    /// node, or inside an unnamed function the nearest enclosing declared
    /// symbol. `None` at module top level.
    pub enclosing_caller_id: Option<String>,
    /// The nearest enclosing symbol, or the `File` node.
    pub enclosing_symbol_id: String,
    /// The enclosing class or interface's `qualifiedName`. A method or
    /// function-valued field is a member node only when this is set.
    pub enclosing_type_qname: Option<String>,
    /// The enclosing class's heritage names.
    pub supertype_names: Vec<String>,
    /// Inside a function body nothing is a graph node.
    pub inside_function: bool,
}

impl Scope {
    /// The module's top level.
    pub fn module(file_id: &str) -> Self {
        Self {
            prefix: Vec::new(),
            namespace_prefix: String::new(),
            locals: None,
            type_parameters: None,
            enclosing_caller_id: None,
            enclosing_symbol_id: file_id.to_string(),
            enclosing_type_qname: None,
            supertype_names: Vec::new(),
            inside_function: false,
        }
    }
}

/// `scope` with `node`'s own `<T, ...>` bound, or `scope` unchanged when it
/// declares none.
pub fn type_parameter_scope(node: Node, source: &str, scope: Scope) -> Scope {
    let Some(list) = node.child_by_field_name("type_parameters") else { return scope };
    let names: HashSet<String> = named_children(list)
        .into_iter()
        .filter(|parameter| parameter.kind() == "type_parameter")
        .filter_map(|parameter| parameter.child_by_field_name("name"))
        .map(|name| text(name, source).to_string())
        .collect();
    if names.is_empty() {
        return scope;
    }
    let parent = scope.type_parameters.clone();
    Scope { type_parameters: Some(Rc::new(LocalBindings { names, parent })), ..scope }
}

// --- the value scope chain ------------------------------------------------
//
// Locals are not graph nodes, so a use of one would otherwise name-match the
// file-level declaration it shadows. The chain follows real JS/TS scoping:
// `var` and function declarations hoist to the function body, `let`, `const`
// and `class` bind per block, so a name shadowed inside one block still
// resolves normally outside it.

/// The chain inside function `function`: its parameters and the `var`s and
/// function declarations hoisted to its body. The function's own name is
/// not bound: for a declaration it is this file's symbol (binding it would
/// erase every recursive call), and a named function expression denotes the
/// symbol it was declared into.
pub fn function_scope(
    function: Node,
    source: &str,
    parent: Option<&Rc<LocalBindings>>,
) -> Option<Rc<LocalBindings>> {
    let mut names = HashSet::new();
    if let Some(parameters) = function.child_by_field_name("parameters") {
        for parameter in named_children(parameters) {
            collect_pattern_names(parameter, source, &mut names);
        }
    }
    // An arrow function's one unparenthesized parameter (`x => ...`).
    if let Some(parameter) = function.child_by_field_name("parameter") {
        collect_pattern_names(parameter, source, &mut names);
    }
    if let Some(body) = function.child_by_field_name("body") {
        collect_hoisted_bindings(body, source, &mut names);
    }
    Some(Rc::new(LocalBindings { names, parent: parent.cloned() }))
}

/// `scope` with the block-scoped declarations written directly in `block`
/// (a `{ ... }` or a `switch` body) bound, or `scope` when it declares none.
pub fn block_scope(block: Node, source: &str, scope: &Scope) -> Scope {
    let mut names = HashSet::new();
    collect_block_bindings(block, source, &mut names);
    if names.is_empty() {
        return scope.clone();
    }
    bound_scope(names, scope)
}

/// `scope` with `names` bound, as a catch clause or a `for` header binds them.
pub fn bound_scope(names: HashSet<String>, scope: &Scope) -> Scope {
    Scope { locals: Some(Rc::new(LocalBindings { names, parent: scope.locals.clone() })), ..scope.clone() }
}

/// Node types the hoisting pre-scan does not descend into: no statement can
/// hide under one except inside a function or class body, where the scan
/// stops anyway.
const NO_STATEMENTS_INSIDE: [&str; 12] = [
    "arguments",
    "array",
    "binary_expression",
    "call_expression",
    "comment",
    "member_expression",
    "object",
    "string",
    "template_string",
    "type_annotation",
    "type_arguments",
    "type_parameters",
];

/// The `var`s and function declarations under `node`, however deeply nested
/// in blocks, but never inside a nested function or class.
pub fn collect_hoisted_bindings(node: Node, source: &str, into: &mut HashSet<String>) {
    for child in named_children(node) {
        if NO_STATEMENTS_INSIDE.contains(&child.kind()) {
            continue;
        }
        match child.kind() {
            "function_declaration" | "generator_function_declaration" => {
                if let Some(name) = child.child_by_field_name("name") {
                    into.insert(text(name, source).to_string());
                }
                continue;
            }
            "arrow_function"
            | "function_expression"
            | "generator_function"
            | "method_definition"
            | "class_declaration"
            | "abstract_class_declaration"
            | "class" => continue,
            // `var`, the one declaration form that hoists.
            "variable_declaration" => collect_declaration_names(child, source, into),
            _ => {}
        }
        collect_hoisted_bindings(child, source, into);
    }
}

/// The block-scoped declarations written directly in `block`. A `switch`
/// body is one scope shared by every case.
pub fn collect_block_bindings(block: Node, source: &str, into: &mut HashSet<String>) {
    for child in named_children(block) {
        match child.kind() {
            "lexical_declaration" => collect_declaration_names(child, source, into),
            "class_declaration"
            | "abstract_class_declaration"
            | "function_declaration"
            | "generator_function_declaration" => {
                if let Some(name) = child.child_by_field_name("name") {
                    into.insert(text(name, source).to_string());
                }
            }
            "switch_case" | "switch_default" => collect_block_bindings(child, source, into),
            _ => {}
        }
    }
}

/// Whether a `for...of`/`for...in` header declares its loop variable rather
/// than assigning to an existing name.
pub fn declares_binding(statement: Node) -> bool {
    children(statement).iter().any(|child| matches!(child.kind(), "var" | "let" | "const"))
}

/// Every name the declarators of a `var`/`let`/`const` statement bind.
pub fn collect_declaration_names(declaration: Node, source: &str, into: &mut HashSet<String>) {
    for declarator in named_children(declaration) {
        if declarator.kind() != "variable_declarator" {
            continue;
        }
        if let Some(name) = declarator.child_by_field_name("name") {
            collect_pattern_names(name, source, into);
        }
    }
}

/// Every name a binding pattern introduces. Only the binding side is
/// walked: a default value is evaluated in the enclosing scope.
pub fn collect_pattern_names(node: Node, source: &str, into: &mut HashSet<String>) {
    match node.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            into.insert(text(node, source).to_string());
        }
        "required_parameter" | "optional_parameter" => {
            if let Some(pattern) = node.child_by_field_name("pattern") {
                collect_pattern_names(pattern, source, into);
            }
        }
        "assignment_pattern" | "object_assignment_pattern" => {
            if let Some(left) = node.child_by_field_name("left") {
                collect_pattern_names(left, source, into);
            }
        }
        "pair_pattern" => {
            if let Some(value) = node.child_by_field_name("value") {
                collect_pattern_names(value, source, into);
            }
        }
        "object_pattern" | "array_pattern" | "rest_pattern" => {
            for child in named_children(node) {
                collect_pattern_names(child, source, into);
            }
        }
        // `for (const [k, v] of ...)`: the loop variable arrives declared.
        "lexical_declaration" | "variable_declaration" => collect_declaration_names(node, source, into),
        _ => {}
    }
}

/// Whether identifier `node` introduces a binding rather than uses a name.
/// A JSX element's name and a generic type's head sit in a `name` field but
/// are uses.
pub fn is_binding_position(node: Node) -> bool {
    let Some(parent) = node.parent() else { return false };
    if parent.kind().starts_with("jsx_") || parent.kind() == "generic_type" {
        return false;
    }
    let is_field =
        |field: &str| parent.child_by_field_name(field).is_some_and(|child| child.id() == node.id());
    if ["name", "pattern", "alias"].into_iter().any(is_field) {
        return true;
    }
    parent.kind() == "for_in_statement" && is_field("left")
}
