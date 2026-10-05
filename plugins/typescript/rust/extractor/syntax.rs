//! Reading the syntax tree: node text, literals, heritage clauses,
//! signatures and doc comments.

use tree_sitter::Node;

/// A node's source text.
pub fn text<'s>(node: Node, source: &'s str) -> &'s str {
    source.get(node.byte_range()).unwrap_or("")
}

/// A node's named children, in order.
pub fn named_children(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

/// A node's children, named and anonymous, in order.
pub fn children(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.children(&mut cursor).collect()
}

/// Whether any child (anonymous tokens included) is of `kind`: `static`,
/// `async`, `get`, `set`, `*`.
pub fn has_child_of_kind(node: Node, kind: &str) -> bool {
    children(node).iter().any(|child| child.kind() == kind)
}

/// The first child (anonymous tokens included) of `kind`.
pub fn child_of_kind<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    children(node).into_iter().find(|child| child.kind() == kind)
}

/// The text of a string literal without its quotes, when the literal is
/// exactly its own text: one plain run of characters. A template
/// substitution or an escape sequence splits it into parts this cannot read,
/// and the answer is `None`.
pub fn string_literal_value(node: Node, source: &str) -> Option<String> {
    if node.kind() != "string" && node.kind() != "template_string" {
        return None;
    }
    let parts = named_children(node);
    if parts.iter().any(|part| part.kind() != "string_fragment") {
        return None;
    }
    if parts.is_empty() {
        // `""` has no fragment child at all.
        let whole = text(node, source);
        let mut chars = whole.chars();
        return (whole.chars().count() >= 2).then(|| {
            chars.next();
            chars.next_back();
            chars.as_str().to_string()
        });
    }
    Some(parts.iter().map(|part| text(*part, source)).collect())
}

/// The supertype names a `class_heritage` or `extends_type_clause` lists,
/// type arguments dropped (`Base<T>` -> `Base`), qualified names kept whole
/// (`NS.Base`).
pub fn heritage_names(clause: Node, source: &str) -> Vec<String> {
    fn collect(node: Node, source: &str, names: &mut Vec<String>) {
        match node.kind() {
            "extends_clause" | "implements_clause" => {
                for child in named_children(node) {
                    collect(child, source, names);
                }
            }
            "generic_type" => {
                if let Some(name) = node.child_by_field_name("name") {
                    names.push(text(name, source).to_string());
                }
            }
            "identifier" | "type_identifier" | "nested_type_identifier" | "member_expression" => {
                names.push(text(node, source).to_string());
            }
            _ => {}
        }
    }
    let mut names = Vec::new();
    for child in named_children(clause) {
        collect(child, source, &mut names);
    }
    names
}

/// Whether a declaration carries an implementation. A `const` or class-field
/// declarator holds its function in `value`, so the body is looked for there
/// too.
pub fn has_body(node: Node) -> bool {
    if node.child_by_field_name("body").is_some() {
        return true;
    }
    node.child_by_field_name("value").is_some_and(|value| value.child_by_field_name("body").is_some())
}

/// A method's `nativeKind`. A signature-only method is `method`, like its
/// implementation, so an overloaded method stays one node; getters, setters,
/// constructors and abstract methods are distinct symbols.
pub fn method_native_kind(node: Node, name: &str, is_static: bool) -> &'static str {
    if node.kind() == "abstract_method_signature" {
        return "abstract_method";
    }
    if has_child_of_kind(node, "get") {
        return "getter";
    }
    if has_child_of_kind(node, "set") {
        return "setter";
    }
    if !is_static && name == "constructor" {
        return "constructor";
    }
    "method"
}

/// `name<T>(params): Return`, rebuilt from the declaration's own fields with
/// every whitespace run collapsed to one space.
pub fn function_signature(name: &str, node: Node, source: &str) -> String {
    let is_async = has_child_of_kind(node, "async");
    let type_parameters = node.child_by_field_name("type_parameters").map_or("", |n| text(n, source));
    let parameters = match node.child_by_field_name("parameters") {
        Some(parameters) => text(parameters, source).to_string(),
        // An arrow function with one unparenthesized parameter.
        None => match node.child_by_field_name("parameter") {
            Some(parameter) => format!("({})", text(parameter, source)),
            None => "()".to_string(),
        },
    };
    let return_type = node.child_by_field_name("return_type").map_or("", |n| text(n, source));
    let signature =
        format!("{}{name}{type_parameters}{parameters}{return_type}", if is_async { "async " } else { "" });
    collapse_whitespace(&signature)
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The `/** ... */` block immediately before `node`, delimiters and `*`
/// gutter stripped. Line comments and plain block comments are not docs.
pub fn doc_comment_for(node: Node, source: &str) -> Option<String> {
    let previous = node.prev_named_sibling()?;
    if previous.kind() != "comment" {
        return None;
    }
    let raw = text(previous, source);
    if !raw.starts_with("/**") {
        return None;
    }
    let end = if raw.ends_with("*/") { raw.len() - 2 } else { raw.len() };
    let inner = if end > 3 { &raw[3..end] } else { "" };
    let body = inner
        .split('\n')
        .map(|line| {
            let line = line.trim_start();
            line.strip_prefix('*').map_or(line, |rest| rest.trim_start_matches('*')).trim()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let body = body.trim();
    (!body.is_empty()).then(|| body.to_string())
}
