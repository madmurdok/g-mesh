//! Decision 1: the lexical scope stack, and why a structural extractor
//! cannot do without one.
//!
//! # The trap
//!
//! Every edge this plugin emits for a bare name starts by asking "is this
//! name a declaration somewhere else". If nothing tracks *local* bindings,
//! the answer for
//!
//! ```rust,ignore
//! fn run(config: Config) {
//!     let parse = 1;
//!     helper(parse);
//! }
//! ```
//!
//! is that `parse` is a name the module might declare, and the extractor
//! emits a module-scoped placeholder for a local integer. Core then links it
//! to whatever the module really calls `parse` - a function, a module, a
//! constant - and `find_references` reports a use that does not exist. This
//! is not a hypothetical: it is the shape every structural extractor gets
//! wrong first, and the reason it is worth its own module is that the fix has
//! to be *complete*. One unhandled binding form is one silent class of wrong
//! edges.
//!
//! # What is tracked
//!
//! A stack of frames. Anything that can introduce a name pushes one, and
//! every name a pattern binds goes into the top frame:
//!
//!  - function, method and closure parameters, including `self`;
//!  - `let` bindings (after their own initializer has been visited, so that
//!    `let x = x;` still refers to the outer `x`);
//!  - `for` patterns, `match` arm patterns, `if let`/`while let` patterns;
//!  - generic type parameters and const generics, on functions, impls,
//!    traits and type declarations - so that `T` in `fn f<T>(t: T)` is never
//!    mistaken for a type the project declares;
//!  - every block, so a binding leaves scope with its block.
//!
//! Lifetimes are not tracked: they live in a namespace of their own that no
//! edge this plugin emits ever addresses.
//!
//! # What it does when it is unsure
//!
//! [`Scopes::binds`] answering `true` means "do not emit an edge for this
//! name". Every binding form above adds names; nothing ever removes one
//! except leaving its frame. So the failure direction is *over*-binding - a
//! name shadowed more widely than Rust would shadow it - which costs a
//! missing edge. That is the standing rule (`graph::symbol_links`: "a missing
//! edge beats a wrong one") and it is why, for instance, a `tuple_struct_pattern`'s
//! own type (`Some(x)`, `Point(a, b)`) is skipped rather than resolved: the
//! variant it names is worth an edge in principle, but telling a unit variant
//! from a binding needs name resolution, and guessing either way here would
//! trade a missing edge for a wrong one.

use std::collections::HashSet;

use tree_sitter::Node;

use crate::extractor::syntax::text;

/// The bindings in scope at one point of a walk, innermost frame last.
#[derive(Debug, Default)]
pub(crate) struct Scopes {
    frames: Vec<HashSet<String>>,
}

impl Scopes {
    /// An empty stack - a file's top level, where nothing is bound.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Enters a scope. Every caller pairs this with [`Scopes::pop`]; the two
    /// are not a guard type because the walk is recursive and a guard would
    /// have to borrow the stack for the whole of each recursive call.
    pub(crate) fn push(&mut self) {
        self.frames.push(HashSet::new());
    }

    /// Leaves the innermost scope.
    pub(crate) fn pop(&mut self) {
        self.frames.pop();
    }

    /// Binds one name in the innermost scope. A name bound at the file's top
    /// level (no frame at all) is ignored rather than panicking: that is a
    /// syntax error's partial tree, and the file's other declarations are
    /// still worth having.
    pub(crate) fn bind(&mut self, name: &str) {
        if let Some(frame) = self.frames.last_mut() {
            frame.insert(name.to_string());
        }
    }

    /// Whether `name` is bound by anything lexically enclosing this point -
    /// that is, whether an edge for it would be an edge for a local.
    pub(crate) fn binds(&self, name: &str) -> bool {
        self.frames.iter().any(|frame| frame.contains(name))
    }

    /// Binds every name `pattern` introduces.
    ///
    /// Walks the pattern rather than matching each of the dozen pattern node
    /// kinds, because the ones that *do not* bind are the short list: a
    /// pattern's own type path (`Some(x)`, `Point { x }`), a struct pattern's
    /// field names, literals, and `_`.
    pub(crate) fn bind_pattern(&mut self, pattern: Node, source: &str) {
        match pattern.kind() {
            "identifier" => self.bind(text(pattern, source)),
            // `Config { host, port }` binds `host` and `port`; the `host:` in
            // `Config { host: h }` is a field name and `h` is the binding,
            // which the generic recursion below reaches as an `identifier`.
            "shorthand_field_identifier" => self.bind(text(pattern, source)),
            "field_identifier" => {}
            _ => {
                let mut cursor = pattern.walk();
                for child in pattern.named_children(&mut cursor) {
                    // The `type` of a tuple-struct or struct pattern is the
                    // variant being matched, not a binding - see the module
                    // doc for why it is skipped rather than referenced.
                    if pattern.child_by_field_name("type").is_some_and(|ty| ty == child) {
                        continue;
                    }
                    self.bind_pattern(child, source);
                }
            }
        }
    }

    /// Binds every generic parameter a `type_parameters` node declares -
    /// `T`, `const N: usize`. Lifetimes are skipped (see the module doc).
    pub(crate) fn bind_type_parameters(&mut self, parameters: Node, source: &str) {
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            if parameter.kind() == "lifetime" {
                continue;
            }
            // `type_parameter`/`const_parameter`/`optional_type_parameter`
            // put the name in `name`; `constrained_type_parameter` puts it in
            // `left` (its `bounds` are real type references and are walked
            // like any other type).
            let name = parameter
                .child_by_field_name("name")
                .or_else(|| parameter.child_by_field_name("left"))
                .filter(|node| matches!(node.kind(), "type_identifier" | "identifier"));
            if let Some(name) = name {
                self.bind(text(name, source));
            }
        }
    }

    /// Binds every name a parameter list introduces, `self` included.
    pub(crate) fn bind_parameters(&mut self, parameters: Node, source: &str) {
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            match parameter.kind() {
                "self_parameter" => self.bind("self"),
                _ => {
                    if let Some(pattern) = parameter.child_by_field_name("pattern") {
                        self.bind_pattern(pattern, source);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_rust::LANGUAGE.into()).unwrap();
        parser.parse(source, None).unwrap()
    }

    fn first<'t>(tree: &'t tree_sitter::Tree, kind: &str) -> Node<'t> {
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == kind {
                return node;
            }
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
        panic!("no {kind} in the tree");
    }

    fn bound(source: &str, kind: &str, bind: fn(&mut Scopes, Node, &str)) -> Scopes {
        let tree = parse(source);
        let node = first(&tree, kind);
        let mut scopes = Scopes::new();
        scopes.push();
        bind(&mut scopes, node, source);
        scopes
    }

    #[test]
    fn a_binding_is_only_in_scope_inside_its_frame() {
        let mut scopes = Scopes::new();
        assert!(!scopes.binds("x"));
        scopes.push();
        scopes.bind("x");
        assert!(scopes.binds("x"));
        scopes.push();
        assert!(scopes.binds("x"), "an inner frame still sees an outer binding");
        scopes.pop();
        scopes.pop();
        assert!(!scopes.binds("x"));
    }

    #[test]
    fn destructuring_patterns_bind_every_name_they_introduce() {
        let scopes = bound(
            "fn f() { let Config { host, port: p, .. } = c; }",
            "let_declaration",
            |scopes, node, source| scopes.bind_pattern(node.child_by_field_name("pattern").unwrap(), source),
        );
        assert!(scopes.binds("host"), "a shorthand field binds its own name");
        assert!(scopes.binds("p"), "a renamed field binds the new name");
        assert!(!scopes.binds("port"), "the field name itself is not a binding");
        assert!(!scopes.binds("Config"), "the pattern's type is not a binding");
    }

    #[test]
    fn a_tuple_struct_pattern_binds_its_fields_and_not_its_variant() {
        let scopes = bound("fn f() { let Some(inner) = x; }", "let_declaration", |scopes, node, source| {
            scopes.bind_pattern(node.child_by_field_name("pattern").unwrap(), source)
        });
        assert!(scopes.binds("inner"));
        assert!(!scopes.binds("Some"));
    }

    #[test]
    fn parameters_bind_their_patterns_and_self() {
        let scopes = bound(
            "impl T { fn f(&self, (a, b): (u8, u8), c: u8) {} }",
            "parameters",
            |scopes, node, source| scopes.bind_parameters(node, source),
        );
        for name in ["self", "a", "b", "c"] {
            assert!(scopes.binds(name), "{name} must be bound");
        }
    }

    #[test]
    fn generic_parameters_are_bound_and_lifetimes_are_not() {
        let scopes =
            bound("fn f<'a, T, U: Clone, const N: usize>() {}", "type_parameters", |scopes, node, source| {
                scopes.bind_type_parameters(node, source)
            });
        for name in ["T", "U", "N"] {
            assert!(scopes.binds(name), "{name} must be bound");
        }
        assert!(!scopes.binds("a"), "a lifetime is not in the type namespace");
        assert!(!scopes.binds("Clone"), "a bound is a real type reference, not a binding");
    }
}
