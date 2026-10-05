//! The lexical context threaded through the walk.

use std::collections::HashSet;
use std::rc::Rc;

use g_mesh_plugin_sdk::wire::PathSegment;
use tree_sitter::Node;

use crate::extractor::syntax::{named_children, text};

/// One link of a scope chain: the names a function, block or declaration
/// binds, and the link enclosing it.
#[derive(Debug)]
pub struct LocalBindings {
    pub names: HashSet<String>,
    pub parent: Option<Rc<LocalBindings>>,
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
