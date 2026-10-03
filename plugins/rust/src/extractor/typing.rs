//! Typed receivers: the type of a local, when this file spells it out.
//!
//! A receiver call `x.m()` gets a structural edge only when `x`'s type `T` is
//! known from what this same file writes, and the edge is then addressed
//! exactly as `T::m(x)` is (see [`super::bodies`]). The design and the
//! measurements behind it are `docs/architecture/gm-485-local-receiver-types.md`.
//!
//! # What types a local
//!
//! - **A written type**: a parameter's (`fn f(x: &T)`, `|x: T|`), a `let`'s
//!   (`let x: T`), a struct literal's (`let x = T { .. }`).
//! - **A written return type** of a function or method this file declares:
//!   `let x = f()`, `let x = T::f()`, and one hop further, `let y = x.m()`
//!   where `x` is itself typed. A chain of such hops stops at [`MAX_HOPS`].
//! - **An alias**: `let y = x`, `let y = &x`.
//!
//! # What a written type is reduced to
//!
//! `&`, `&mut`, lifetimes and `Box<T>` are stripped. `Option<T>` and
//! `Result<T, _>` are remembered as a [`Wrapper`] and come off only through an
//! explicit `?`, `.unwrap()` or `.expect(..)`. Those three heads count as the
//! standard library's only when this file neither declares nor imports a
//! project item of that name. `Rc<T>`/`Arc<T>` are not dereferenced: their own
//! methods (`clone`, `downgrade`) would otherwise be addressed at `T`.
//!
//! Everything else stays untyped and keeps only its open site: generic
//! parameters, `dyn`/`impl` types, tuples, types this project does not declare
//! or import by name (`String`, `Vec<T>`, a name reached through a glob
//! import), and every binding a pattern destructures.

use tree_sitter::Node;

use crate::extractor::syntax::{flatten_path, text, Seg};

/// How many method-return hops may separate a local from a written type: `let
/// a = x.m(); let b = a.n();` is two.
pub(crate) const MAX_HOPS: u8 = 2;

/// One segment of a written type's path, owned so it can outlive the walk
/// that read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathSeg {
    Crate,
    SelfMod,
    Super,
    Name(String),
}

/// A type as written, reduced to a path and its generic arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WrittenType {
    pub(crate) path: Vec<PathSeg>,
    /// The type arguments in order, lifetimes and associated-type bindings
    /// left out. `None` for an argument that is not a path type.
    pub(crate) args: Vec<Option<WrittenType>>,
}

impl WrittenType {
    /// The written type at `node`, or `None` for anything but a (possibly
    /// borrowed) path type.
    pub(crate) fn parse(node: Node, source: &str) -> Option<Self> {
        match node.kind() {
            "reference_type" => Self::parse(node.child_by_field_name("type")?, source),
            "type_identifier" | "scoped_type_identifier" => {
                Some(Self { path: owned(&flatten_path(node, source)?), args: Vec::new() })
            }
            "generic_type" => {
                let path = owned(&flatten_path(node.child_by_field_name("type")?, source)?);
                let mut args = Vec::new();
                if let Some(list) = node.child_by_field_name("type_arguments") {
                    let mut cursor = list.walk();
                    for arg in list.named_children(&mut cursor) {
                        if matches!(arg.kind(), "lifetime" | "type_binding" | "trait_bounds") {
                            continue;
                        }
                        args.push(Self::parse(arg, source));
                    }
                }
                Some(Self { path, args })
            }
            _ => None,
        }
    }

    /// The path as the resolver's borrowed segments.
    pub(crate) fn segments(&self) -> Vec<Seg<'_>> {
        self.path
            .iter()
            .map(|seg| match seg {
                PathSeg::Crate => Seg::Crate,
                PathSeg::SelfMod => Seg::SelfMod,
                PathSeg::Super => Seg::Super,
                PathSeg::Name(name) => Seg::Name(name),
            })
            .collect()
    }

    /// The name of a one-segment path.
    pub(crate) fn single(&self) -> Option<&str> {
        match self.path.as_slice() {
            [PathSeg::Name(name)] => Some(name),
            _ => None,
        }
    }

    /// The same type with every one-segment `Self` replaced by `self_type`.
    /// `None` when `Self` appears and there is no self type to put there, or
    /// when `Self` heads a longer path (`Self::Item`).
    pub(crate) fn substitute_self(mut self, self_type: Option<&str>) -> Option<Self> {
        if self.single() == Some("Self") {
            self.path = vec![PathSeg::Name(self_type?.to_string())];
        } else if matches!(self.path.first(), Some(PathSeg::Name(name)) if name == "Self") {
            return None;
        }
        let mut args = Vec::with_capacity(self.args.len());
        for arg in self.args {
            args.push(match arg {
                Some(arg) => Some(arg.substitute_self(self_type)?),
                None => None,
            });
        }
        self.args = args;
        Some(self)
    }

    /// Whether any one-segment name in this type satisfies `pred`.
    pub(crate) fn mentions(&self, pred: &dyn Fn(&str) -> bool) -> bool {
        self.single().is_some_and(pred) || self.args.iter().flatten().any(|arg| arg.mentions(pred))
    }
}

fn owned(segments: &[Seg<'_>]) -> Vec<PathSeg> {
    segments
        .iter()
        .map(|seg| match seg {
            Seg::Crate => PathSeg::Crate,
            Seg::SelfMod => PathSeg::SelfMod,
            Seg::Super => PathSeg::Super,
            Seg::Name(name) => PathSeg::Name((*name).to_string()),
        })
        .collect()
}

/// What still has to come off a typed value before it is a `T`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wrapper {
    Plain,
    Option,
    Result,
}

/// Where a local's type was read from. Carried for measurement and tests;
/// nothing branches on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Origin {
    Parameter,
    ClosureParameter,
    LetAnnotation,
    StructLiteral,
    FreeFnReturn,
    AssocFnReturn,
    MethodReturn,
}

/// A local's type: `name` in `container`, the same address `T::m()` uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalType {
    pub(crate) container: String,
    pub(crate) name: String,
    pub(crate) wrapper: Wrapper,
    /// Method-return hops from a written type - see [`MAX_HOPS`].
    pub(crate) hops: u8,
    pub(crate) origin: Origin,
    /// Whether a `?`/`unwrap()`/`expect()` produced this type.
    pub(crate) unwrapped: bool,
}

/// The name a binding pattern introduces, when it introduces exactly one
/// name and nothing else: `x` and `mut x`.
pub(crate) fn binding_name<'s>(pattern: Node, source: &'s str) -> Option<&'s str> {
    match pattern.kind() {
        "identifier" => Some(text(pattern, source)),
        "mut_pattern" => {
            let mut cursor = pattern.walk();
            let inner =
                pattern.named_children(&mut cursor).find(|child| child.kind() != "mutable_specifier")?;
            (inner.kind() == "identifier").then(|| text(inner, source))
        }
        _ => None,
    }
}

/// Every generic parameter name in scope at `item`: its own and those of
/// every enclosing `fn`, `impl` and `trait`.
pub(crate) fn generic_names(item: Node, source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut node = Some(item);
    while let Some(current) = node {
        if matches!(current.kind(), "function_item" | "function_signature_item" | "impl_item" | "trait_item")
        {
            if let Some(parameters) = current.child_by_field_name("type_parameters") {
                let mut cursor = parameters.walk();
                for parameter in parameters.named_children(&mut cursor) {
                    let name = parameter
                        .child_by_field_name("name")
                        .or_else(|| parameter.child_by_field_name("left"))
                        .filter(|name| matches!(name.kind(), "type_identifier" | "identifier"));
                    if let Some(name) = name {
                        names.push(text(name, source).to_string());
                    }
                }
            }
        }
        node = current.parent();
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(source: &str) -> Option<WrittenType> {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_rust::LANGUAGE.into()).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "function_item" {
                return WrittenType::parse(node.child_by_field_name("return_type").unwrap(), source);
            }
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor).collect::<Vec<_>>());
        }
        panic!("no function in {source:?}");
    }

    fn name(ty: &WrittenType) -> &str {
        ty.single().expect("a one-segment path")
    }

    #[test]
    fn references_and_lifetimes_are_stripped() {
        assert_eq!(name(&written("fn f<'a>() -> &'a mut T {}").unwrap()), "T");
    }

    #[test]
    fn generic_arguments_are_kept_in_order_without_lifetimes() {
        let ty = written("fn f<'a>() -> Result<&'a T, E> {}").unwrap();
        assert_eq!(name(&ty), "Result");
        let args: Vec<&str> = ty.args.iter().map(|arg| name(arg.as_ref().unwrap())).collect();
        assert_eq!(args, ["T", "E"]);
    }

    #[test]
    fn a_type_that_is_not_a_path_has_no_written_form() {
        for source in
            ["fn f() -> (A, B) {}", "fn f() -> impl Tr {}", "fn f() -> &dyn Tr {}", "fn f() -> [u8; 4] {}"]
        {
            assert_eq!(written(source), None, "{source}");
        }
    }

    #[test]
    fn self_is_replaced_and_self_paths_are_refused() {
        let ty = written("fn f() -> Option<Self> {}").unwrap().substitute_self(Some("P")).unwrap();
        assert_eq!(name(ty.args[0].as_ref().unwrap()), "P");
        assert_eq!(written("fn f() -> Self {}").unwrap().substitute_self(None), None);
        assert_eq!(written("fn f() -> Self::Item {}").unwrap().substitute_self(Some("P")), None);
    }

    #[test]
    fn mentions_reaches_into_arguments() {
        let ty = written("fn f() -> Option<T> {}").unwrap();
        assert!(ty.mentions(&|name| name == "T"));
        assert!(!ty.mentions(&|name| name == "U"));
    }
}
