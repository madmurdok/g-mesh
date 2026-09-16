//! The thin layer between tree-sitter-python's concrete syntax tree and the
//! questions the rest of this extractor asks of it: what does this dotted
//! name spell, what is this definition's docstring, what is its rendered
//! signature (decorators and all), and which definition does a
//! `decorated_definition` wrap.
//!
//! # Why a segment list rather than a string
//!
//! Almost every decision this plugin makes about an edge starts with "what
//! does this dotted name mean here": `a.b.f()`, `mod.Thing`, `self.method()`,
//! `from a.b import name`. Written out as a string, every one of those has to
//! be re-split at `.` by whoever reads it - and the split is not innocent,
//! because Python spells three different things with the same dot. `a.b` is a
//! module path in an `import` statement, an attribute access in an
//! expression, and part of a *relative* path when a leading run of dots
//! precedes it. [`dotted_segments`] answers once, in the grammar's own terms:
//! it returns segments only for shapes that really are a chain of plain
//! identifiers, and `None` for everything else.
//!
//! # What "no path" means
//!
//! [`dotted_segments`] returns `None` for anything it cannot render as a
//! `.`-joined chain of identifiers: a subscript (`registry["name"].run`), a
//! call result (`factory().run`), a literal's method (`"".join`), a
//! parenthesised expression. That is deliberately the same answer as "I do
//! not know", and every caller treats it that way - it emits no edge, or
//! records an open site for a future semantic tier. A guess here would be a
//! wrong edge, which this project ranks below a missing one.
//!
//! # Docstrings, and why they are not comments
//!
//! Python has no doc-comment syntax. `# like this` is a comment the compiler
//! discards; the documentation of a module, class or function is its
//! **first statement**, when that statement is a string literal, and it
//! survives into `__doc__` at runtime. So [`docstring`] reads exactly that -
//! the first statement of a body - and nothing above the definition is ever
//! read as documentation. A `#` comment sitting above a `def` documents it to
//! a human and to nothing else, and treating it as a `docComment` would put
//! text in the index that Python itself does not consider the symbol's
//! documentation.

use tree_sitter::Node;

/// The source text a node covers. `""` for the impossible case of a node
/// whose bytes are not valid UTF-8 - impossible because `extract` is handed a
/// `&str`, and stated as a default rather than an `unwrap` because a panic
/// here would cost the whole file (the SDK catches it, but the file's graph
/// is gone).
pub(crate) fn text<'s>(node: Node, source: &'s str) -> &'s str {
    node.utf8_text(source.as_bytes()).unwrap_or("")
}

/// The chain of identifiers `node` spells, or `None` when it spells something
/// this plugin deliberately does not interpret - see the module doc.
///
/// Handles the three shapes Python writes a dotted name in: a bare
/// `identifier`, the `dotted_name` an `import` statement uses, and the
/// `attribute` chain an expression uses. They are three different node kinds
/// for one idea, which is exactly why a caller should not have to know which
/// one it is looking at.
pub(crate) fn dotted_segments<'s>(node: Node, source: &'s str) -> Option<Vec<&'s str>> {
    match node.kind() {
        "identifier" => Some(vec![text(node, source)]),
        "dotted_name" => {
            let mut cursor = node.walk();
            let segments: Vec<&str> = node
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "identifier")
                .map(|child| text(child, source))
                .collect();
            (!segments.is_empty()).then_some(segments)
        }
        "attribute" => {
            let object = node.child_by_field_name("object")?;
            let attribute = node.child_by_field_name("attribute")?;
            // `attribute` is a `field_identifier`-equivalent here: the
            // grammar gives it kind `identifier`, but a non-identifier
            // attribute is impossible, so anything else is a partial tree.
            if attribute.kind() != "identifier" {
                return None;
            }
            let mut segments = dotted_segments(object, source)?;
            segments.push(text(attribute, source));
            Some(segments)
        }
        _ => None,
    }
}

/// The name a `function_definition`/`class_definition` declares.
pub(crate) fn definition_name<'s>(item: Node, source: &'s str) -> Option<&'s str> {
    Some(text(item.child_by_field_name("name")?, source))
}

/// The definition a `decorated_definition` wraps, or `item` itself when it is
/// already a plain definition.
///
/// The grammar wraps a decorated `def`/`class` in an outer node whose
/// `definition` field holds the real one. Every caller wants the inner node
/// (it carries `name`, `parameters`, `body`) *and* the outer one's extent
/// (the range and the signature have to include the decorators), which is why
/// this is a function rather than a match at each call site.
pub(crate) fn inner_definition(item: Node) -> Node {
    if item.kind() == "decorated_definition" {
        item.child_by_field_name("definition").unwrap_or(item)
    } else {
        item
    }
}

/// Every `decorator` node attached to `item`, in source order. Empty for an
/// undecorated definition.
pub(crate) fn decorators(item: Node) -> Vec<Node> {
    if item.kind() != "decorated_definition" {
        return Vec::new();
    }
    let mut cursor = item.walk();
    item.named_children(&mut cursor).filter(|child| child.kind() == "decorator").collect()
}

/// The name of a definition's **first parameter**, when it has one that is a
/// plain name.
///
/// This is what lets `self.helper()` inside a method resolve without the
/// plugin ever trusting the *name* `self`. `self` is not a Python keyword -
/// it is a convention for what to call a method's first parameter, and a
/// function may name it anything (`cls` in a `@classmethod`, `s` in code that
/// ignores PEP 8). What is structural, and what this reads, is that the first
/// parameter of a method *is* the instance the method was called on. See
/// [`super::bodies`]' own Decision 7 for the full argument and for the
/// `@staticmethod` exception, which has no instance parameter at all.
///
/// `None` for an empty parameter list, and for a first parameter that is
/// `*args`/`**kwargs` or a destructuring pattern - none of which is an
/// instance.
pub(crate) fn first_parameter_name<'s>(definition: Node, source: &'s str) -> Option<&'s str> {
    let parameters = definition.child_by_field_name("parameters")?;
    let mut cursor = parameters.walk();
    let first = parameters.named_children(&mut cursor).next()?;
    match first.kind() {
        "identifier" => Some(text(first, source)),
        "typed_parameter" => {
            let mut cursor = first.walk();
            let name = first.named_children(&mut cursor).find(|child| child.kind() == "identifier");
            name.map(|child| text(child, source))
        }
        "default_parameter" | "typed_default_parameter" => {
            let name = first.child_by_field_name("name")?;
            (name.kind() == "identifier").then(|| text(name, source))
        }
        _ => None,
    }
}

/// Whether `item` carries a decorator whose *name* is `name` - matched on the
/// decorator's own dotted path, so `@staticmethod` matches and
/// `@functools.wraps(staticmethod)` does not.
pub(crate) fn has_decorator(item: Node, source: &str, name: &str) -> bool {
    decorators(item).into_iter().any(|decorator| {
        decorator
            .named_child(0)
            .and_then(|expression| dotted_segments(expression, source))
            .is_some_and(|segments| segments.last() == Some(&name))
    })
}

/// The text a string literal holds, verbatim - the source between its opening
/// and closing delimiters. Used for an `__all__` entry, where the name is
/// wanted exactly as written and no docstring cleanup applies.
pub(crate) fn string_literal<'s>(literal: Node, source: &'s str) -> Option<&'s str> {
    (literal.kind() == "string").then(|| string_content(literal, source)).flatten()
}

/// The docstring of a body - the first statement of a `module` or `block`,
/// when that statement is a string literal.
///
/// Returns the string's *content*, with PEP 257's own cleanup applied (see
/// [`cleandoc`]). `None` when the first statement is anything else, including
/// a comment: see the module doc for why a comment is never a docstring.
pub(crate) fn docstring(body: Node, source: &str) -> Option<String> {
    let mut cursor = body.walk();
    let first = body.named_children(&mut cursor).next()?;
    if first.kind() != "expression_statement" {
        return None;
    }
    let literal = first.named_child(0)?;
    if literal.kind() != "string" {
        return None;
    }
    cleandoc(string_content(literal, source)?)
}

/// The text between a string literal's opening and closing delimiters.
///
/// Read from the delimiters' own positions rather than by concatenating the
/// `string_content` children, because the children are interrupted by
/// `escape_sequence` and `interpolation` nodes and stitching them back
/// together would silently drop a `\n` written as an escape. What a
/// `docComment` wants is the source the author wrote, which is exactly this
/// slice.
fn string_content<'s>(literal: Node, source: &'s str) -> Option<&'s str> {
    let mut cursor = literal.walk();
    let children: Vec<Node> = literal.children(&mut cursor).collect();
    let start = children.iter().find(|child| child.kind() == "string_start")?;
    let end = children.iter().rev().find(|child| child.kind() == "string_end")?;
    source.get(start.end_byte()..end.start_byte())
}

/// PEP 257's own docstring cleanup, the rule `inspect.cleandoc` implements:
/// the first line is stripped of leading whitespace, every following line is
/// dedented by the *smallest* indentation any of them has, and leading and
/// trailing blank lines go.
///
/// This is not cosmetic. A docstring's indentation is a fact about where its
/// `def` sits, not about the prose - so a function moved into a class, or a
/// block re-indented, would otherwise change the `docComment` of a symbol
/// whose documentation nobody edited. Applying Python's own rule makes the
/// stored text invariant under exactly the edits Python itself considers
/// irrelevant.
///
/// Returns `None` for a docstring that is empty once cleaned, so an empty
/// `""""""` is an absent doc rather than an empty one - the same distinction
/// the Rust plugin's `join_doc` makes.
fn cleandoc(raw: &str) -> Option<String> {
    /// How many leading *characters* of `line` are whitespace. Characters,
    /// not bytes: a docstring indented with a non-breaking space (which
    /// `str::trim_start` does trim) would otherwise make the dedent below
    /// slice in the middle of a multi-byte character and panic, costing the
    /// whole file's graph for a stray keystroke.
    fn leading_whitespace(line: &str) -> usize {
        line.chars().take_while(|ch| ch.is_whitespace()).count()
    }

    let mut lines: Vec<String> = raw.lines().map(str::to_string).collect();
    if let Some(first) = lines.first_mut() {
        *first = first.trim_start().to_string();
    }
    let indent = lines
        .iter()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .map(|line| leading_whitespace(line))
        .min()
        .unwrap_or(0);
    for line in lines.iter_mut().skip(1) {
        let cut = indent.min(leading_whitespace(line));
        *line = line.chars().skip(cut).collect::<String>().trim_end().to_string();
    }
    while lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    let joined = lines.join("\n");
    (!joined.trim().is_empty()).then_some(joined)
}

/// The declaration's rendered signature: everything up to its body, on one
/// line, **including its decorators**.
///
/// # Why decorators are in the signature
///
/// The task asks for them to be "recorded in signature", and the reason is
/// that in Python a decorator is the closest thing a declaration has to a
/// modifier: `@property`, `@staticmethod`, `@classmethod`, `@abstractmethod`
/// and `@dataclass` each change what the name *is*, not merely what it does.
/// A reader who sees `def area(self)` without its `@property` has been told
/// something false about how to use it. They are deliberately not in the
/// `nativeKind` - that is part of the node's id, and a decorator added or
/// removed would then delete the symbol and add a stranger, taking every
/// inbound edge with it.
///
/// It is cut at the definition's `body` field, so a 200-line function renders
/// as its header. Whitespace runs collapse to one space, which is not only
/// cosmetic: it makes the signature invariant under the reformatting edits
/// that move nothing else - a parameter list broken across lines by a
/// formatter is the same signature - which is what keeps a plugin's answer to
/// a whitespace-only edit an empty diff (`id-stability.whitespace-edit`).
pub(crate) fn signature(item: Node, source: &str) -> Option<String> {
    let inner = inner_definition(item);
    let start = item.start_byte();
    let end = inner.child_by_field_name("body").map(|body| body.start_byte()).unwrap_or(item.end_byte());
    let slice = source.get(start..end.max(start))?;
    let rendered = collapse_whitespace(slice.trim().trim_end_matches(':').trim());
    (!rendered.is_empty()).then_some(rendered)
}

/// The signature of a module-level assignment: its target and, when it has
/// one, its annotation - never its value.
///
/// `MAX_RETRIES: int = 3` renders as `MAX_RETRIES: int`; `TABLE = {…200
/// lines…}` renders as `TABLE`. The value is excluded for the reason the Rust
/// plugin excludes a `static`'s initializer: a signature is what a result
/// page shows and what `search_code` embeds, and a literal data table is
/// neither.
pub(crate) fn assignment_signature(name: &str, assignment: Node, source: &str) -> String {
    match assignment.child_by_field_name("type") {
        Some(annotation) => format!("{name}: {}", collapse_whitespace(text(annotation, source))),
        None => name.to_string(),
    }
}

/// The same rendering [`signature`] uses, for text that is not a definition.
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

/// Whether `name` is spelled the way Python spells a class rather than a
/// module or a function.
///
/// # A convention, used only where it cannot produce a wrong edge
///
/// Python's grammar does not say whether the `b` in `a.b.f()` is a module or
/// a class - only the import system does, at runtime, and this tier has no
/// runtime. The one honest signal is the naming convention PEP 8 states and
/// every linter enforces: a class is `CapWords`, a module and a function are
/// `lower_case`.
///
/// It is used to choose between two *addresses*, never to decide whether to
/// emit an edge. Guessing "class" for a module makes the placeholder address
/// a `qualifiedName` key nothing declares, so the edge stays unresolved;
/// guessing "module" for a class makes it address a container no member ever
/// joins, so the edge stays unresolved. Both mistakes cost a missing edge and
/// neither can produce a wrong one, which is the only footing on which a
/// convention is allowed to decide anything here.
pub(crate) fn looks_like_class(name: &str) -> bool {
    name.chars().next().is_some_and(|first| first.is_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_python::LANGUAGE.into()).unwrap();
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
    fn the_three_spellings_of_a_dotted_name_all_flatten() {
        let tree = parse("import a.b.c\n");
        let source = "import a.b.c\n";
        assert_eq!(dotted_segments(first(&tree, "dotted_name"), source), Some(vec!["a", "b", "c"]));

        let source = "x = a.b.c\n";
        let tree = parse(source);
        assert_eq!(dotted_segments(first(&tree, "attribute"), source), Some(vec!["a", "b", "c"]));

        let source = "x = a\n";
        let tree = parse(source);
        let identifier = first(&tree, "assignment").child_by_field_name("right").unwrap();
        assert_eq!(dotted_segments(identifier, source), Some(vec!["a"]));
    }

    /// A shape this plugin deliberately does not interpret answers `None`, so
    /// every caller reaches its "I do not know" branch rather than a
    /// plausible-looking wrong one.
    #[test]
    fn a_subscript_or_a_call_result_is_not_a_dotted_name() {
        let source = "x = registry[\"k\"].run\n";
        let tree = parse(source);
        assert_eq!(dotted_segments(first(&tree, "attribute"), source), None);

        let source = "x = factory().run\n";
        let tree = parse(source);
        assert_eq!(dotted_segments(first(&tree, "attribute"), source), None);
    }

    #[test]
    fn a_docstring_is_the_first_statement_and_a_comment_is_not() {
        let source = "\"\"\"Module doc.\"\"\"\ndef f():\n    \"\"\"Doc of f.\"\"\"\n    pass\n";
        let tree = parse(source);
        assert_eq!(docstring(tree.root_node(), source).as_deref(), Some("Module doc."));
        let body = first(&tree, "function_definition").child_by_field_name("body").unwrap();
        assert_eq!(docstring(body, source).as_deref(), Some("Doc of f."));

        let source = "# not a docstring\ndef f():\n    pass\n";
        let tree = parse(source);
        assert_eq!(docstring(tree.root_node(), source), None);
        let body = first(&tree, "function_definition").child_by_field_name("body").unwrap();
        assert_eq!(docstring(body, source), None);
    }

    /// PEP 257's rule: the continuation lines are dedented by the smallest
    /// indentation among them, so re-indenting the `def` does not rewrite the
    /// doc.
    #[test]
    fn a_multi_line_docstring_is_dedented_the_way_python_dedents_it() {
        let source = "def f():\n    \"\"\"One.\n\n    Two.\n        Deeper.\n    \"\"\"\n    pass\n";
        let tree = parse(source);
        let body = first(&tree, "function_definition").child_by_field_name("body").unwrap();
        assert_eq!(docstring(body, source).as_deref(), Some("One.\n\nTwo.\n    Deeper."));
    }

    #[test]
    fn an_empty_docstring_is_an_absent_one() {
        let source = "def f():\n    \"\"\"   \"\"\"\n    pass\n";
        let tree = parse(source);
        let body = first(&tree, "function_definition").child_by_field_name("body").unwrap();
        assert_eq!(docstring(body, source), None);
    }

    #[test]
    fn a_signature_carries_the_decorators_and_stops_at_the_body() {
        let source =
            "@cache\n@app.route(\"/x\")\ndef handler(\n    request,\n    *,\n    retries: int = 3,\n) -> Response:\n    return None\n";
        let tree = parse(source);
        let item = first(&tree, "decorated_definition");
        assert_eq!(
            signature(item, source).as_deref(),
            Some("@cache @app.route(\"/x\") def handler( request, *, retries: int = 3, ) -> Response")
        );
    }

    #[test]
    fn a_class_signature_carries_its_bases() {
        let source = "class Greeter(Base, metaclass=Meta):\n    pass\n";
        let tree = parse(source);
        assert_eq!(
            signature(first(&tree, "class_definition"), source).as_deref(),
            Some("class Greeter(Base, metaclass=Meta)")
        );
    }

    #[test]
    fn an_assignment_signature_keeps_the_annotation_and_drops_the_value() {
        let source = "TABLE: dict[str, int] = {\n    \"a\": 1,\n}\n";
        let tree = parse(source);
        let assignment = first(&tree, "assignment");
        assert_eq!(assignment_signature("TABLE", assignment, source), "TABLE: dict[str, int]");

        let source = "TABLE = {\"a\": 1}\n";
        let tree = parse(source);
        assert_eq!(assignment_signature("TABLE", first(&tree, "assignment"), source), "TABLE");
    }

    #[test]
    fn the_naming_convention_separates_classes_from_modules() {
        assert!(looks_like_class("Greeter"));
        assert!(!looks_like_class("greeter"));
        assert!(!looks_like_class("_private"));
        assert!(!looks_like_class(""));
    }
}
