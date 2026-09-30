//! The `structured` text form - a doc comment trimmed to its prose
//! outline before the signature is appended.
//!
//! Deterministic and purely syntactic: no model, no network. The input is a
//! stored doc comment (comment markers already stripped by the indexer); the
//! rules apply in this order:
//!
//! 1. **Fenced code** - a line whose trimmed text starts with ` ``` ` or
//!    `~~~` opens a fence, the next line starting with the same marker closes
//!    it; both and everything between are dropped (an unclosed fence runs to
//!    the end). The fence also ends the paragraph it sits in.
//! 2. **Paragraphs** - what is left is split at whitespace-only lines. A
//!    Markdown ATX heading (a line starting, unindented, with 1-6 `#` and a
//!    space) is always a paragraph of its own, even with no blank line
//!    around it.
//! 3. **Dropped sections** - a heading whose text (lowercased, trailing `:`
//!    removed) is in [`DROPPED_SECTIONS`] (`# Arguments`, `# Returns`,
//!    `# Errors`, `# Panics`, `# Examples`, ...) is dropped together with
//!    every paragraph after it, up to the next heading of the same or a
//!    higher level. A paragraph whose first line is one such name alone
//!    followed by `:` or `::` (Google-style `Args:`, `Raises:`; reST
//!    `Usage::`, `Example:`) is dropped whole.
//! 4. **Code paragraphs** - a paragraph whose every non-blank line is
//!    indented by a tab or four spaces (Markdown/Go/reST indented code) and
//!    whose first line is not a list item, or whose first line starts with
//!    `>>>` (a Python doctest), is dropped. Indented list continuations are
//!    not code: their paragraph starts with the list marker.
//! 5. **Tag and field lists** - inside a paragraph, a line starting with
//!    `@` and a letter (JSDoc `@param`, `@returns`, `@throws`, ...) or with
//!    a reST field (`:param x:`, `:rtype:`, `:raises E:`) is dropped with
//!    every line after it in that paragraph: the lines that follow a tag are
//!    its continuation or further tags.
//! 6. **Link-only lines** - a Markdown reference definition
//!    (`` [`Foo`]: https://... ``) or a line that is one URL, optionally in
//!    `<>` and optionally after `See:`/`See`, is dropped.
//! 7. **Short prose** - of the paragraphs still standing, headings are kept,
//!    the first prose paragraph is kept whole (the summary), and every later
//!    one is kept only when it is at most [`SHORT_PARAGRAPH_CHARS`]
//!    characters long.
//!
//! Kept paragraphs are rejoined with a blank line, in their original order.

/// The "short prose" cap: a paragraph after the first is kept only when its
/// text (lines joined by `\n`, ends trimmed) is at most this many `char`s,
/// about two and a half wrapped lines of doc prose.
pub(super) const SHORT_PARAGRAPH_CHARS: usize = 200;

/// Section names whose heading, or label line, drops the section: the
/// parameter/return/error lists and the examples. Lowercase, without the
/// trailing `:`.
pub(super) const DROPPED_SECTIONS: &[&str] = &[
    "arguments",
    "args",
    "parameters",
    "params",
    "returns",
    "return",
    "errors",
    "panics",
    "raises",
    "throws",
    "yields",
    "examples",
    "example",
    "usage",
];

/// `doc` trimmed by the rules in this module's doc; empty when nothing
/// survives (a doc that is only code, say).
pub(super) fn structured_doc(doc: &str) -> String {
    let mut kept: Vec<String> = Vec::new();
    let mut have_summary = false;
    // The level of the dropped heading whose section is being skipped.
    let mut skipping: Option<usize> = None;
    for block in paragraphs(doc.trim()) {
        if let Some(level) = heading_level(block[0]) {
            if skipping.is_some_and(|skip| level > skip) {
                continue;
            }
            skipping = None;
            if is_dropped_name(heading_text(block[0])) {
                skipping = Some(level);
            } else {
                kept.push(block[0].trim().to_string());
            }
            continue;
        }
        if skipping.is_some() || is_label_line(block[0]) || is_code(&block) {
            continue;
        }
        let lines: Vec<&str> = block
            .iter()
            .copied()
            .take_while(|line| !is_tag_line(line))
            .filter(|line| !is_link_only(line))
            .collect();
        let text = lines.join("\n");
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        if !have_summary || text.chars().count() <= SHORT_PARAGRAPH_CHARS {
            kept.push(text.to_string());
        }
        have_summary = true;
    }
    kept.join("\n\n")
}

/// The doc's lines split into paragraphs: fenced code removed, blank lines
/// as separators, each ATX heading a paragraph of its own.
fn paragraphs(doc: &str) -> Vec<Vec<&str>> {
    let mut out: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut fence: Option<&str> = None;
    for line in doc.lines() {
        let trimmed = line.trim_start();
        if let Some(marker) = fence {
            if trimmed.starts_with(marker) {
                fence = None;
            }
            continue;
        }
        let marker = ["```", "~~~"].into_iter().find(|m| trimmed.starts_with(m));
        if marker.is_some() || line.trim().is_empty() || heading_level(line).is_some() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            if let Some(marker) = marker {
                fence = Some(marker);
            } else if heading_level(line).is_some() {
                out.push(vec![line]);
            }
            continue;
        }
        current.push(line);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// The level of a Markdown ATX heading: 1-6 `#` at the very start of the
/// line, then a space and some text.
fn heading_level(line: &str) -> Option<usize> {
    let level = line.bytes().take_while(|&b| b == b'#').count();
    let rest = &line[level..];
    ((1..=6).contains(&level) && rest.starts_with(' ') && !rest.trim().is_empty()).then_some(level)
}

fn heading_text(line: &str) -> &str {
    line.trim_start_matches('#').trim()
}

fn is_dropped_name(name: &str) -> bool {
    let name = name.trim().trim_end_matches(':').trim().to_ascii_lowercase();
    DROPPED_SECTIONS.contains(&name.as_str())
}

/// `Args:`, `Returns:`, `Usage::` - a dropped section name alone on the line.
fn is_label_line(line: &str) -> bool {
    let line = line.trim();
    line.ends_with(':') && is_dropped_name(line)
}

fn is_code(block: &[&str]) -> bool {
    if block[0].trim_start().starts_with(">>>") {
        return true;
    }
    !is_list_item(block[0])
        && block
            .iter()
            .filter(|line| !line.trim().is_empty())
            .all(|line| line.starts_with('\t') || line.starts_with("    "))
}

fn is_list_item(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ") {
        return true;
    }
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && (t[digits..].starts_with(". ") || t[digits..].starts_with(") "))
}

/// A JSDoc tag (`@param`) or a reST field (`:param x:`, `:rtype:`).
fn is_tag_line(line: &str) -> bool {
    let t = line.trim_start();
    let mut chars = t.chars();
    match chars.next() {
        Some('@') => chars.next().is_some_and(|c| c.is_ascii_alphabetic()),
        Some(':') => {
            let rest = chars.as_str();
            let name = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
            name > 0 && rest[name..].contains(':')
        }
        _ => false,
    }
}

/// A reference definition, or a line that is one URL (optionally in `<>`,
/// optionally after `See:`/`See`).
fn is_link_only(line: &str) -> bool {
    let t = line.trim();
    if t.starts_with('[') {
        if let Some(end) = t.find("]:") {
            let target = t[end + 2..].trim();
            return end > 1 && !target.is_empty() && !target.contains(char::is_whitespace);
        }
    }
    let t = t.strip_prefix("See:").or_else(|| t.strip_prefix("See ")).unwrap_or(t).trim();
    let t = t.strip_prefix('<').and_then(|s| s.strip_suffix('>')).unwrap_or(t);
    (t.starts_with("https://") || t.starts_with("http://")) && !t.contains(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every test names its control: the code change that must make it fail.

    /// Control: making `structured_doc` return `doc.trim().to_string()`
    /// fails this and every drop test below.
    #[test]
    fn keeps_first_paragraph_headings_and_short_prose() {
        let long = "word ".repeat(50);
        let doc = format!(
            "Summary line\ncontinued.\n\n# Why\n\nShort reason.\n\n{long}\n\n## How it works\n\nAlso short."
        );
        assert_eq!(
            structured_doc(&doc),
            "Summary line\ncontinued.\n\n# Why\n\nShort reason.\n\n## How it works\n\nAlso short."
        );
    }

    /// The first paragraph is kept whole, later ones only up to the cap.
    /// Control: dropping `!have_summary ||` from the keep condition fails the
    /// first assertion; changing `<=` to `<` fails the second.
    #[test]
    fn cap_applies_after_the_first_paragraph_only() {
        let long = "a".repeat(SHORT_PARAGRAPH_CHARS + 1);
        let exact = "b".repeat(SHORT_PARAGRAPH_CHARS);
        assert_eq!(structured_doc(&format!("{long}\n\n{long}")), long);
        assert_eq!(structured_doc(&format!("S.\n\n{exact}\n\n{long}")), format!("S.\n\n{exact}"));
    }

    /// Control: removing the fence branch in `paragraphs` (so fenced lines
    /// are ordinary lines) fails it.
    #[test]
    fn drops_fenced_code() {
        let doc = "Escapes bytes.\n\n```\nuse grep_cli::escape;\n\nassert_eq!(x, y);\n```\nAfter.\n\n~~~rust\nlet a = 1;\n~~~";
        assert_eq!(structured_doc(doc), "Escapes bytes.\n\nAfter.");
        // An unclosed fence runs to the end.
        assert_eq!(structured_doc("S.\n\n```\ncode\n\nmore code"), "S.");
    }

    /// Control: making `is_code` return `false` fails it; dropping the
    /// `!is_list_item` guard fails the list-continuation case.
    #[test]
    fn drops_indented_code_and_doctests_but_not_list_continuations() {
        let go = "Param returns the value.\n\n\trouter.GET(\"/user/:id\", func(c *gin.Context) {\n\t    id := c.Param(\"id\")\n\t})";
        assert_eq!(structured_doc(go), "Param returns the value.");
        let py = "Sends it.\n\n>>> import requests\n>>> requests.get(url)";
        assert_eq!(structured_doc(py), "Sends it.");
        let md = "S.\n\n    let x = 1;\n    let y = 2;";
        assert_eq!(structured_doc(md), "S.");
        let list = "S.\n\n  - an item\n    continued";
        assert_eq!(structured_doc(list), "S.\n\n- an item\n    continued");
    }

    /// Control: making `is_dropped_name` return `false` fails it.
    #[test]
    fn drops_rust_parameter_and_example_sections() {
        let doc = "Opens it.\n\n# Arguments\n\n* `path` - where.\n\n# Errors\n\nWhen missing.\n\n# Safety\n\nCall once.\n\n# Examples\n\nThis shows it.\n\n## Detail\n\nNested, still dropped.\n\n# Notes\n\nKept.";
        assert_eq!(structured_doc(doc), "Opens it.\n\n# Safety\n\nCall once.\n\n# Notes\n\nKept.");
        // A heading directly followed by text, with no blank line.
        assert_eq!(structured_doc("S.\n\n# Panics\nIf empty.\n# Notes\nKept."), "S.\n\n# Notes\n\nKept.");
    }

    /// Control: making `is_label_line` return `false` fails it.
    #[test]
    fn drops_python_label_sections() {
        let google = "Parses it.\n\nArgs:\n    text: the input.\n\nReturns:\n    The tree.\n\nRaises:\n    ValueError: when bad.";
        assert_eq!(structured_doc(google), "Parses it.");
        let rest = "The adapter.\n\nUsage::\n\n  >>> import requests\n  >>> s = requests.Session()";
        assert_eq!(structured_doc(rest), "The adapter.");
    }

    /// Control: making `is_tag_line` return `false` fails it.
    #[test]
    fn drops_jsdoc_tags_and_rest_fields() {
        let js = "Calculates the deltas.\n\n@param prev - Previous.\n@param next - Next.\n\n@returns The delta\n  spanning lines.";
        assert_eq!(structured_doc(js), "Calculates the deltas.");
        let js_inline = "Updates it.\n@param a first\n@returns new instance";
        assert_eq!(structured_doc(js_inline), "Updates it.");
        let py = "Sends it.\n\n:param request: The request.\n:param timeout: How long\n    to wait.\n:rtype: Response";
        assert_eq!(structured_doc(py), "Sends it.");
    }

    /// Control: making `is_link_only` return `false` fails it.
    #[test]
    fn drops_link_only_lines() {
        let doc = "Reads it.\n\nSee: <https://learn.microsoft.com/wsl>\n\nMore in [`None`].\n[`None`]: https://doc.rust-lang.org/std/option\nhttps://example.com/x";
        assert_eq!(structured_doc(doc), "Reads it.\n\nMore in [`None`].");
        // A line that merely contains a link is prose.
        assert_eq!(structured_doc("See https://x.y for details."), "See https://x.y for details.");
    }

    /// Control: making `structured_doc` return `doc.to_string()` fails the
    /// code-only case; a panic on an empty `paragraphs` result fails the
    /// empty ones.
    #[test]
    fn empty_and_code_only_docs_yield_nothing() {
        assert_eq!(structured_doc(""), "");
        assert_eq!(structured_doc("  \n\t\n"), "");
        assert_eq!(structured_doc("```\nfn main() {}\n```"), "");
        assert_eq!(structured_doc("# Examples\n\nIt runs.\n\n```\nrun();\n```"), "");
    }

    /// Same input, same output; no state crosses calls. Control: any
    /// iteration over a `HashMap`/`HashSet` or a clock in the rules makes
    /// this flaky (here: comparing two calls and a fixed expectation).
    #[test]
    fn is_deterministic() {
        let doc = "S.\n\n# Why\n\nShort.\n\n@param x y";
        let first = structured_doc(doc);
        for _ in 0..16 {
            assert_eq!(structured_doc(doc), first);
        }
        assert_eq!(first, "S.\n\n# Why\n\nShort.");
    }
}
