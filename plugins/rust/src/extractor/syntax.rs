//! The thin layer between tree-sitter-rust's concrete syntax tree and the
//! questions the rest of this extractor asks of it: what is this item's name,
//! its doc comment, its rendered signature, and - the one that does real work -
//! what path does this node spell.
//!
//! # Why a `Seg` list rather than a string
//!
//! Almost every decision this plugin makes about an edge starts with "what
//! does this path mean here": `crate::a::b::f()`, `super::g`, `Point::new`,
//! `fmt::Display`. Written out as a string, every one of those has to be
//! re-split at `::` by whoever reads it, and the two segments that are *not*
//! identifiers - `crate` and `self`, which are keywords the grammar gives
//! their own node kinds - become indistinguishable from a module that happens
//! to be called `crate`. [`flatten_path`] answers once, in the grammar's own
//! terms, and [`Seg`] keeps the keywords separate from the names by type. The
//! resolution rules in [`keys`](crate::extractor::keys) then read as the
//! language's own rules rather than as string surgery.
//!
//! # What "no path" means
//!
//! [`flatten_path`] returns `None` for anything it cannot render as a
//! `::`-joined chain of identifiers and path keywords: `<T as Trait>::m`
//! written out in source, `&T`, `(A, B)`, `[u8; 4]`, a leading `::` with no
//! path of its own. That is deliberately the same answer as "I do not know",
//! and every caller treats it that way - it emits no edge, or records an open
//! site for the semantic tier. A guess here would be a wrong edge, which this
//! project ranks below a missing one.

use tree_sitter::Node;

/// The source text a node covers. `""` for the impossible case of a node
/// whose bytes are not valid UTF-8 - impossible because `extract` is handed a
/// `&str`, and stated as a default rather than an `unwrap` because a panic
/// here would cost the whole file (the SDK catches it, but the file's graph
/// is gone).
pub(crate) fn text<'s>(node: Node, source: &'s str) -> &'s str {
    node.utf8_text(source.as_bytes()).unwrap_or("")
}

/// One segment of a Rust path, with the three path keywords kept apart from
/// ordinary names because they are resolved against the *asking* module
/// rather than looked up by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Seg<'s> {
    /// `crate` - the root of the crate the asking file belongs to.
    Crate,
    /// `self` - the asking module itself.
    SelfMod,
    /// `super` - the asking module's parent. Repeats (`super::super::x`) walk
    /// up one ancestor each.
    Super,
    /// An ordinary identifier: a module, a type, a function, a crate name.
    Name(&'s str),
}

impl Seg<'_> {
    /// The name, for a segment that is one. `None` for a path keyword, which
    /// is never looked up by name.
    pub(crate) fn name(&self) -> Option<&str> {
        match self {
            Seg::Name(name) => Some(name),
            _ => None,
        }
    }
}

/// The path `node` spells, or `None` when it spells something this plugin
/// deliberately does not interpret - see the module doc.
///
/// Type arguments are dropped (`Point::<u8>::new` is `Point::new`,
/// `Vec<T>` is `Vec`): they are not part of a container key or a
/// `qualifiedName`, and keeping them would make `Point::<u8>::new()` and
/// `Point::<u16>::new()` address two different, both nonexistent, symbols.
pub(crate) fn flatten_path<'s>(node: Node, source: &'s str) -> Option<Vec<Seg<'s>>> {
    match node.kind() {
        "identifier" | "type_identifier" | "field_identifier" | "shorthand_field_identifier" => {
            Some(vec![Seg::Name(text(node, source))])
        }
        "crate" => Some(vec![Seg::Crate]),
        "self" => Some(vec![Seg::SelfMod]),
        "super" => Some(vec![Seg::Super]),
        "scoped_identifier" | "scoped_type_identifier" => {
            // No `path` field is the leading-`::` form (`::krate::item`),
            // which names an external crate root this plugin does not model.
            let path = node.child_by_field_name("path")?;
            let name = node.child_by_field_name("name")?;
            let mut segments = flatten_path(path, source)?;
            segments.extend(flatten_path(name, source)?);
            Some(segments)
        }
        "generic_type" => flatten_path(node.child_by_field_name("type")?, source),
        _ => None,
    }
}

/// A path's own name - its last segment - for the common case where only that
/// is wanted. `None` for a path whose last segment is a keyword, which no
/// declaration is ever called.
pub(crate) fn path_tail<'s>(segments: &[Seg<'s>]) -> Option<&'s str> {
    match segments.last() {
        Some(Seg::Name(name)) => Some(name),
        _ => None,
    }
}

/// The name a declaration node carries, from whichever field the grammar puts
/// it in - `name` for every item this plugin declares.
pub(crate) fn item_name<'s>(item: Node, source: &'s str) -> Option<&'s str> {
    Some(text(item.child_by_field_name("name")?, source))
}

/// The doc comment attached *to* `item`: the run of `///` (or `/** */`)
/// comments immediately above it, with attribute items allowed in between.
///
/// Attributes are stepped over rather than stopping the run because
/// `/// doc` + `#[derive(Debug)]` + `struct S` is ordinary Rust and the doc
/// plainly belongs to `S`. Anything else - a blank line is not a node, but a
/// non-doc `//` comment or another item is - ends the run, so a comment
/// paragraph that belongs to the *previous* item is never stolen.
pub(crate) fn outer_doc_comment(item: Node, source: &str) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut sibling = item.prev_sibling();
    while let Some(node) = sibling {
        match node.kind() {
            "attribute_item" => {}
            "line_comment" | "block_comment" => match doc_text(node, source) {
                Some((DocKind::Outer, line)) => lines.push(line),
                // An inner doc (`//!`) documents the enclosing module, and a
                // plain `//` comment documents nothing the index carries.
                _ => break,
            },
            _ => break,
        }
        sibling = node.prev_sibling();
    }
    lines.reverse();
    join_doc(lines)
}

/// The doc comment attached *inside* `container` - the leading `//!` run of a
/// `source_file` or of a `mod x { … }` body, which documents the module
/// itself rather than anything after it.
pub(crate) fn inner_doc_comment(container: Node, source: &str) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cursor = container.walk();
    for child in container.children(&mut cursor) {
        match child.kind() {
            "{" => continue,
            "line_comment" | "block_comment" => match doc_text(child, source) {
                Some((DocKind::Inner, line)) => lines.push(line),
                // An outer doc at the top of a module body belongs to the
                // first item, not to the module.
                _ => break,
            },
            _ => break,
        }
    }
    join_doc(lines)
}

enum DocKind {
    Outer,
    Inner,
}

/// The text a doc comment carries, and which of the two kinds it is. `None`
/// for an ordinary comment, which the grammar marks by giving it no `doc`
/// child at all.
fn doc_text(comment: Node, source: &str) -> Option<(DocKind, String)> {
    let doc = comment.child_by_field_name("doc")?;
    let kind = if comment.child_by_field_name("inner").is_some() {
        DocKind::Inner
    } else if comment.child_by_field_name("outer").is_some() {
        DocKind::Outer
    } else {
        return None;
    };
    // rustdoc's own rule: the marker is followed by one optional space that
    // belongs to the syntax rather than to the prose, and the newline the
    // grammar includes in the `doc` child is the comment's terminator.
    let body = text(doc, source).trim_end_matches(['\n', '\r']);
    Some((kind, body.strip_prefix(' ').unwrap_or(body).to_string()))
}

fn join_doc(lines: Vec<String>) -> Option<String> {
    let joined = lines.join("\n");
    (!joined.trim().is_empty()).then_some(joined)
}

/// The declaration's rendered signature: everything up to its body, on one
/// line.
///
/// # Why it stops where it does, and why whitespace is collapsed
///
/// The signature is what `search_code` embeds and what a caller reads in a
/// result page, so it is the *header* - `pub fn new(x: T, y: T) -> Self`, not
/// the function. It is cut at the `body` field (a function's block, a
/// struct's field list, a trait's declaration list) or, for a `const`/`static`
/// with an initializer, at the `value` field, because a 400-line `static
/// TABLE: [u8; N] = [ … ]` is not a signature.
///
/// Whitespace runs collapse to one space. That is not only cosmetic: it makes
/// the signature invariant under the reformatting edits that move nothing
/// else, which is what keeps a plugin's answer to a whitespace-only edit an
/// empty diff (`id-stability.whitespace-edit`) rather than a field that
/// changed for no reason a reader could see.
pub(crate) fn signature(item: Node, source: &str) -> Option<String> {
    let start = item.start_byte();
    let end = item
        .child_by_field_name("body")
        .or_else(|| item.child_by_field_name("value"))
        .map(|body| body.start_byte())
        .unwrap_or_else(|| item.end_byte());
    let slice = source.get(start..end.max(start))?;
    let rendered = collapse_whitespace(slice.trim().trim_end_matches([';', '=']).trim());
    (!rendered.is_empty()).then_some(rendered)
}

/// The same rendering [`signature`] uses, for a node that is not an item -
/// an `impl` block's self type when it is not a plain path (`&T`,
/// `(A, B)`), which still has to be *named* to give its methods a stable
/// `qualifiedName`.
pub(crate) fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            in_space = true;
            continue;
        }
        if in_space && !out.is_empty() {
            out.push(' ');
        }
        in_space = false;
        out.push(ch);
    }
    out
}

/// Whether `name` is spelled the way Rust spells a type rather than a module
/// or a function.
///
/// # A convention, used only where it cannot produce a wrong edge
///
/// Rust's own grammar does not say whether the `b` in `a::b::f()` is a module
/// or a type - only name resolution does, which is the semantic tier's job
/// (GM-290). The structural tier has one honest signal, and it is the naming
/// convention every Rust codebase follows and `rustc` itself lints for
/// (`non_camel_case_types`, `non_snake_case`): a type is `UpperCamelCase`, a
/// module is `snake_case`.
///
/// It is used to choose between two *addresses*, never to decide whether to
/// emit an edge. Guessing "type" for a module makes the placeholder address
/// `<that module>::b::f`, which nothing declares, so the edge stays
/// unresolved; guessing "module" for a type makes it address container
/// `…::B`, which no member ever joins, so the edge stays unresolved. Both
/// mistakes cost a missing edge and neither can produce a wrong one, which is
/// the only footing on which a convention is allowed to decide anything here.
pub(crate) fn looks_like_type(name: &str) -> bool {
    name.chars().next().is_some_and(|first| first.is_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_rust::LANGUAGE.into()).unwrap();
        parser.parse(source, None).unwrap()
    }

    /// Finds the first node of `kind` in the tree, for a test that wants to
    /// talk about one construct without hand-walking to it.
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

    #[test]
    fn a_scoped_path_flattens_keywords_and_names_apart() {
        let source = "fn f() { crate::a::b::g(); }";
        let tree = parse(source);
        let call = first(&tree, "call_expression");
        let segments = flatten_path(call.child_by_field_name("function").unwrap(), source).unwrap();
        assert_eq!(segments, vec![Seg::Crate, Seg::Name("a"), Seg::Name("b"), Seg::Name("g")]);
    }

    #[test]
    fn turbofish_type_arguments_are_not_path_segments() {
        let source = "fn f() { Point::<u8>::new(); }";
        let tree = parse(source);
        let call = first(&tree, "call_expression");
        let segments = flatten_path(call.child_by_field_name("function").unwrap(), source).unwrap();
        assert_eq!(segments, vec![Seg::Name("Point"), Seg::Name("new")]);
    }

    /// A shape this plugin deliberately does not interpret answers `None`, so
    /// every caller reaches its "I do not know" branch rather than a
    /// plausible-looking wrong one.
    #[test]
    fn a_reference_type_is_not_a_path() {
        let source = "impl Trait for &u8 {}";
        let tree = parse(source);
        let imp = first(&tree, "impl_item");
        assert!(flatten_path(imp.child_by_field_name("type").unwrap(), source).is_none());
    }

    #[test]
    fn an_outer_doc_run_survives_an_attribute_and_stops_at_a_plain_comment() {
        let source = "// not doc\n/// one\n/// two\n#[derive(Debug)]\nstruct S;\n";
        let tree = parse(source);
        let item = first(&tree, "struct_item");
        assert_eq!(outer_doc_comment(item, source).as_deref(), Some("one\ntwo"));
    }

    #[test]
    fn an_inner_doc_documents_its_module_and_an_outer_one_does_not() {
        let source = "//! module doc\n/// item doc\nstruct S;\n";
        let tree = parse(source);
        assert_eq!(inner_doc_comment(tree.root_node(), source).as_deref(), Some("module doc"));
        let item = first(&tree, "struct_item");
        assert_eq!(outer_doc_comment(item, source).as_deref(), Some("item doc"));
    }

    #[test]
    fn a_signature_stops_at_the_body_and_collapses_its_whitespace() {
        let source = "pub fn new(\n    x: T,\n    y: T,\n) -> Self {\n    todo!()\n}\n";
        let tree = parse(source);
        let item = first(&tree, "function_item");
        assert_eq!(signature(item, source).as_deref(), Some("pub fn new( x: T, y: T, ) -> Self"));
    }

    #[test]
    fn a_static_signature_drops_its_initializer() {
        let source = "pub static TABLE: [u8; 2] = [\n    1,\n    2,\n];\n";
        let tree = parse(source);
        let item = first(&tree, "static_item");
        assert_eq!(signature(item, source).as_deref(), Some("pub static TABLE: [u8; 2]"));
    }

    #[test]
    fn the_naming_convention_separates_types_from_modules() {
        assert!(looks_like_type("Point"));
        assert!(!looks_like_type("point"));
        assert!(!looks_like_type("_private"));
        assert!(!looks_like_type(""));
    }
}
