//! Structural context in the embedded text
//! (`docs/architecture/gm-455-structural-context.md`). A variant's text is
//! an unlabelled header - the file path, then the parent, one per line -
//! then a blank line, then `Node::text_for(form)`. An empty header adds
//! nothing, so `ContextForm::None` is the old text byte for byte.
//!
//! `embed_texts` is the one builder: `run` embeds its output and `churn`
//! hashes it, so the churn count cannot drift from the embedded text.

use std::collections::{BTreeSet, HashMap};

use super::config::{ContextForm, TextForm};
use super::rng::Rng;
use super::Node;

/// Where a node's parent line comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParentRef {
    /// A Rust `<X as T>` last segment: `impl T for X`.
    TraitImpl { self_type: String, trait_name: String },
    /// A `Type` node of the same language whose `qualifiedName` is the
    /// remainder (index into the node slice).
    Type(usize),
}

/// A rendered parent line and the names it carries (for `pathOverlap`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parent {
    pub line: String,
    pub names: Vec<String>,
}

/// `(language, qualifiedName)` -> `Type` node indices, in node order.
pub type TypeIndex<'a> = HashMap<(&'a str, &'a str), Vec<usize>>;

pub fn type_index(nodes: &[Node]) -> TypeIndex<'_> {
    let mut index: TypeIndex = HashMap::new();
    for (i, n) in nodes.iter().enumerate().filter(|(_, n)| n.kind == "Type") {
        index.entry((n.language.as_str(), n.qualified_name.as_str())).or_default().push(i);
    }
    index
}

/// `qualifiedName` without `name` and the separator before it (`::`, `.`,
/// `#`); `None` when that leaves nothing or the name is not its suffix.
pub fn owner_of<'a>(qualified_name: &'a str, name: &str) -> Option<&'a str> {
    let rest = qualified_name.strip_suffix(name)?;
    ["::", ".", "#"].iter().find_map(|sep| rest.strip_suffix(sep)).filter(|r| !r.is_empty())
}

/// The last `::` segment of `owner` when it is `<X as T>`, as `(X, T)`.
/// Angle brackets nest (`<Foo<A> as From<B>>`); ` as ` splits at depth 1.
pub fn trait_impl_segment(owner: &str) -> Option<(&str, &str)> {
    if !owner.ends_with('>') {
        return None;
    }
    let bytes = owner.as_bytes();
    let mut depth = 0i32;
    let mut open = None;
    for i in (0..bytes.len()).rev() {
        match bytes[i] {
            b'>' => depth += 1,
            b'<' => {
                depth -= 1;
                if depth == 0 {
                    open = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let open = open?;
    if open != 0 && !owner[..open].ends_with("::") {
        return None;
    }
    let inner = &owner[open + 1..owner.len() - 1];
    let mut depth = 0i32;
    for (i, c) in inner.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ' ' if depth == 0 && inner[i..].starts_with(" as ") => {
                let (x, t) = (inner[..i].trim(), inner[i + 4..].trim());
                return (!x.is_empty() && !t.is_empty()).then_some((x, t));
            }
            _ => {}
        }
    }
    None
}

/// The design's parent rule, from the snapshot alone.
pub fn parent_ref(nodes: &[Node], types: &TypeIndex, i: usize) -> Option<ParentRef> {
    let n = &nodes[i];
    let owner = owner_of(&n.qualified_name, &n.name)?;
    if n.language == "rust" {
        if let Some((x, t)) = trait_impl_segment(owner) {
            return Some(ParentRef::TraitImpl { self_type: x.to_string(), trait_name: t.to_string() });
        }
    }
    let found = types.get(&(n.language.as_str(), owner))?;
    // Same-named types in several files (Python and TS names carry no
    // module): the one in the node's own file, else the first by id.
    let pick = found.iter().copied().find(|&t| nodes[t].file_path == n.file_path).unwrap_or(found[0]);
    Some(ParentRef::Type(pick))
}

pub fn render(nodes: &[Node], parent: &ParentRef) -> Parent {
    match parent {
        ParentRef::TraitImpl { self_type, trait_name } => Parent {
            line: format!("impl {trait_name} for {self_type}"),
            names: vec![self_type.clone(), trait_name.clone()],
        },
        ParentRef::Type(t) => {
            let t = &nodes[*t];
            let line = match t.native_kind.as_deref().filter(|k| !k.is_empty()) {
                Some(kind) => format!("{kind} {}", t.name),
                None => t.name.clone(),
            };
            Parent { line, names: vec![t.name.clone()] }
        }
    }
}

/// Every node's parent, rendered.
pub fn parents(nodes: &[Node]) -> Vec<Option<Parent>> {
    let types = type_index(nodes);
    (0..nodes.len()).map(|i| parent_ref(nodes, &types, i).map(|p| render(nodes, &p))).collect()
}

/// The header lines, joined by `"\n"`, then `"\n\n"`, then `text`; `text`
/// unchanged when there are none.
pub fn with_header(header: &[&str], text: String) -> String {
    if header.is_empty() {
        return text;
    }
    format!("{}\n\n{text}", header.join("\n"))
}

/// Each path in `files` mapped to another one of them, by a seeded
/// derangement, so every file's symbols share one wrong path.
pub fn deranged_paths(files: &[&str], seed: u64) -> HashMap<String, String> {
    let p = Rng::new(seed).derangement(files.len());
    files.iter().enumerate().map(|(i, f)| (f.to_string(), files[p[i]].to_string())).collect()
}

/// `lines` reassigned among themselves by a seeded derangement of their
/// positions: position `i` gets `lines[p[i]]`.
pub fn deranged_lines(lines: &[String], seed: u64) -> Vec<String> {
    let p = Rng::new(seed).derangement(lines.len());
    p.into_iter().map(|j| lines[j].clone()).collect()
}

/// The parent lines' seed, a separate stream from the paths'.
pub fn parent_seed(arm_seed: u64) -> u64 {
    arm_seed.wrapping_add(2)
}

/// The header lines of every node that has a text, and `None` elsewhere.
pub fn headers(
    nodes: &[Node],
    base: &[Option<String>],
    context: ContextForm,
    arm_seed: u64,
) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = vec![Vec::new(); nodes.len()];
    if context == ContextForm::None {
        return out;
    }
    let embedded: Vec<usize> = (0..nodes.len()).filter(|&i| base[i].is_some()).collect();
    let parents = if context.has_parent() { parents(nodes) } else { vec![None; nodes.len()] };

    let path_of: Option<HashMap<String, String>> = context.shuffled().then(|| {
        let files: BTreeSet<&str> = embedded.iter().map(|&i| nodes[i].file_path.as_str()).collect();
        deranged_paths(&files.into_iter().collect::<Vec<_>>(), arm_seed)
    });
    let mut parent_line: Vec<Option<String>> = embedded_parents(&embedded, &parents);
    if context.shuffled() {
        let with: Vec<usize> = (0..nodes.len()).filter(|&i| parent_line[i].is_some()).collect();
        let lines: Vec<String> = with.iter().map(|&i| parent_line[i].clone().unwrap()).collect();
        for (i, line) in with.into_iter().zip(deranged_lines(&lines, parent_seed(arm_seed))) {
            parent_line[i] = Some(line);
        }
    }
    for &i in &embedded {
        let h = &mut out[i];
        if context.has_path() {
            let path = &nodes[i].file_path;
            h.push(path_of.as_ref().map_or_else(|| path.clone(), |m| m[path].clone()));
        }
        if let Some(line) = parent_line[i].take() {
            h.push(line);
        }
    }
    out
}

fn embedded_parents(embedded: &[usize], parents: &[Option<Parent>]) -> Vec<Option<String>> {
    let mut out = vec![None; parents.len()];
    for &i in embedded {
        out[i] = parents[i].as_ref().map(|p| p.line.clone());
    }
    out
}

/// The text every node embeds under `form` and `context`; `None` exactly
/// where `text_for(form)` is, so the candidate set does not depend on the
/// context.
pub fn embed_texts(
    nodes: &[Node],
    form: TextForm,
    context: ContextForm,
    arm_seed: u64,
) -> Vec<Option<String>> {
    let base: Vec<Option<String>> = nodes.iter().map(|n| n.text_for(form)).collect();
    if context == ContextForm::None {
        return base;
    }
    let headers = headers(nodes, &base, context, arm_seed);
    base.into_iter()
        .zip(headers)
        .map(|(text, h)| text.map(|t| with_header(&h.iter().map(String::as_str).collect::<Vec<_>>(), t)))
        .collect()
}

/// A node for tests: `name` is the last segment of `qn`.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) fn test_node(
    id: &str,
    kind: &str,
    native: &str,
    qn: &str,
    file: &str,
    language: &str,
    sig: Option<&str>,
) -> Node {
    let name = qn.rsplit([':', '.', '#']).next().unwrap().to_string();
    let name = if name.ends_with('>') { qn.rsplit("::").next().unwrap().to_string() } else { name };
    Node {
        id: id.into(),
        kind: kind.into(),
        name,
        qualified_name: qn.into(),
        file_path: file.into(),
        language: language.into(),
        native_kind: Some(native.into()),
        container: None,
        start_line: 1,
        end_line: 1,
        text: crate::embedding::pipeline::text_to_embed(None, sig),
        doc: None,
        signature: sig.map(Into::into),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::test_node as node;

    /// Control: always emitting the `"\n\n"` separator (dropping the
    /// `is_empty` early return) fails the first assertion.
    #[test]
    fn an_empty_header_leaves_the_text_unchanged() {
        assert_eq!(with_header(&[], "fn f()".into()), "fn f()");
        assert_eq!(with_header(&["a.rs"], "fn f()".into()), "a.rs\n\nfn f()");
        assert_eq!(with_header(&["a.rs", "struct Foo"], "fn f()".into()), "a.rs\nstruct Foo\n\nfn f()");
    }

    /// Control: removing `embed_texts`' `ContextForm::None` early return
    /// together with `with_header`'s empty-header one fails the first
    /// assertion.
    #[test]
    fn no_context_is_the_form_text_byte_for_byte() {
        let nodes = vec![
            node("1", "Type", "struct", "m::Foo", "a.rs", "rust", Some("pub struct Foo")),
            node("2", "Function", "method", "m::Foo::new", "a.rs", "rust", Some("fn new() -> Foo")),
            node("3", "Function", "function", "m::free", "a.rs", "rust", None),
        ];
        let plain: Vec<_> = nodes.iter().map(|n| n.text_for(TextForm::FirstParagraph)).collect();
        assert_eq!(embed_texts(&nodes, TextForm::FirstParagraph, ContextForm::None, 1), plain);
        let with = embed_texts(&nodes, TextForm::FirstParagraph, ContextForm::PathParent, 1);
        assert_eq!(with[1].as_deref(), Some("a.rs\nstruct Foo\n\nfn new() -> Foo"));
        assert_eq!(with[0].as_deref(), Some("a.rs\n\npub struct Foo"));
        assert_eq!(with[2], None, "no text, no header");
    }

    /// One case per branch of the parent rule. Controls: dropping the
    /// `<X as T>` branch renders the trait-impl method `None` (its owner is
    /// no Type); dropping the language key in `type_index` gives the Go
    /// method the Python class; preferring `found[0]` over the same file
    /// gives `py::Session`'s method the other file's class.
    #[test]
    fn the_parent_rule_per_case() {
        let nodes = vec![
            node("01", "Type", "struct", "decompress::Builder", "d.rs", "rust", None),
            node("02", "Function", "method", "decompress::Builder::new", "d.rs", "rust", Some("fn new()")),
            node(
                "03",
                "Function",
                "trait_impl_method",
                "decompress::<Builder as Default>::default",
                "d.rs",
                "rust",
                Some("fn default()"),
            ),
            node(
                "04",
                "Function",
                "trait_impl_method",
                "m::<Foo<A> as From<io::Error>>::from",
                "d.rs",
                "rust",
                Some("fn from()"),
            ),
            node("05", "Function", "function", "decompress::free", "d.rs", "rust", Some("fn free()")),
            node("06", "Type", "class", "Session", "other.py", "python", None),
            node("07", "Type", "class", "Session", "s.py", "python", None),
            node("08", "Function", "method", "Session.prepare", "s.py", "python", Some("def prepare(self)")),
            node(
                "09",
                "Function",
                "function",
                "Session.prepare.inner",
                "s.py",
                "python",
                Some("def inner()"),
            ),
            node("10", "Function", "method", "Session.run", "s.go", "go", Some("func (s Session) run()")),
            node("11", "Type", "class", "Collab", "c.ts", "typescript", None),
            node("12", "Function", "method", "Collab#mount", "c.ts", "typescript", Some("mount()")),
            node("13", "Type", "struct", "Builder", "top.rs", "rust", None),
        ];
        let p = parents(&nodes);
        let line = |i: usize| p[i].as_ref().map(|p| p.line.as_str());
        assert_eq!(line(1), Some("struct Builder"));
        assert_eq!(line(2), Some("impl Default for Builder"));
        assert_eq!(line(3), Some("impl From<io::Error> for Foo<A>"));
        assert_eq!(line(4), None, "free function");
        assert_eq!(line(7), Some("class Session"));
        let types = type_index(&nodes);
        assert_eq!(parent_ref(&nodes, &types, 7), Some(ParentRef::Type(6)), "same file first");
        assert_eq!(line(8), None, "nested in a function, not a type");
        assert_eq!(line(9), None, "no Go type named Session");
        assert_eq!(line(11), Some("class Collab"));
        assert_eq!(line(12), None, "top-level type");
        assert_eq!(line(0), None, "module-level type");
        assert_eq!(p[2].as_ref().unwrap().names, vec!["Builder".to_string(), "Default".to_string()]);
    }

    /// Controls: replacing Sattolo's derangement with the identity (or a
    /// plain Fisher-Yates shuffle) leaves some path or parent in place;
    /// seeding from anything but `arm_seed` breaks the repeat.
    #[test]
    fn the_shuffled_context_is_a_deterministic_derangement() {
        let mut nodes = Vec::new();
        for f in 0..6 {
            nodes.push(node(
                &format!("t{f}"),
                "Type",
                "struct",
                &format!("m{f}::T{f}"),
                &format!("f{f}.rs"),
                "rust",
                None,
            ));
            for k in 0..3 {
                nodes.push(node(
                    &format!("n{f}{k}"),
                    "Function",
                    "method",
                    &format!("m{f}::T{f}::g{k}"),
                    &format!("f{f}.rs"),
                    "rust",
                    Some(&format!("fn g{k}()")),
                ));
            }
        }
        let real = embed_texts(&nodes, TextForm::FirstParagraph, ContextForm::PathParent, 7);
        let shuffled = embed_texts(&nodes, TextForm::FirstParagraph, ContextForm::PathParentShuffled, 7);
        assert_eq!(
            shuffled,
            embed_texts(&nodes, TextForm::FirstParagraph, ContextForm::PathParentShuffled, 7)
        );
        assert_ne!(
            shuffled,
            embed_texts(&nodes, TextForm::FirstParagraph, ContextForm::PathParentShuffled, 8)
        );
        let mut path_of_file: HashMap<&str, &str> = HashMap::new();
        for (i, (r, s)) in real.iter().zip(&shuffled).enumerate() {
            let (Some(r), Some(s)) = (r, s) else { continue };
            let (r_path, s_path) = (r.lines().next().unwrap(), s.lines().next().unwrap());
            assert_ne!(r_path, s_path, "node {i} kept its path");
            // One wrong path per file.
            assert_eq!(*path_of_file.entry(&nodes[i].file_path).or_insert(s_path), s_path);
            assert_eq!(r.lines().last(), s.lines().last(), "the form text is untouched");
        }
        // Parent lines are a derangement of positions over the 18 nodes that have one.
        let lines: Vec<String> = (0..5).map(|i| format!("l{i}")).collect();
        let d = deranged_lines(&lines, 3);
        assert!(d.iter().zip(&lines).all(|(a, b)| a != b), "{d:?}");
        let mut sorted = d.clone();
        sorted.sort();
        assert_eq!(sorted, lines);
    }
}
