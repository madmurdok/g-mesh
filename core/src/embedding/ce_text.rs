//! The text the cross-encoder reads for one node:
//!
//! ```text
//! <E>\n<P>\n<base>\n<B>
//! ```
//!
//! - `E` (enclosing): the words of the node's native kind (its `kind` when
//!   that is empty), then of its qualified name minus the trailing `name`;
//! - `P` (path): the words of its file path;
//! - `base`: [`text_to_embed`] of its doc comment and signature;
//! - `B` (body): for `Function` and `Type` nodes, the source lines after the
//!   signature, whole lines only, within [`BODY_BUDGET`] tokens.
//!
//! An empty field is left out together with its newline.
//!
//! Invariant: every rule here equals `eval/embedding/gm462_ce_text.py`'s
//! `build(row, src, "EPB", tok, 48)` byte for byte; the reranker's blend
//! weight was measured on that text. `testdata/ce_text.json`, written from
//! the Python, pins it. See ADR 0042.

use std::path::Path;

use super::text::text_to_embed;

/// The body field's token budget.
pub(crate) const BODY_BUDGET: usize = 48;

/// A body line is cut to this many characters (code points).
pub(crate) const LINE_CHARS: usize = 200;

/// The `nodes.kind` values that get a body field.
pub(crate) const BODY_KINDS: [&str; 2] = ["Function", "Type"];

/// The `nodes` columns the text is built from.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CeNode<'a> {
    pub kind: Option<&'a str>,
    pub native_kind: Option<&'a str>,
    pub name: Option<&'a str>,
    pub qualified_name: Option<&'a str>,
    pub file_path: Option<&'a str>,
    /// 0-based, inclusive.
    pub start_line: usize,
    /// 0-based, inclusive.
    pub end_line: usize,
    pub language: Option<&'a str>,
    pub signature: Option<&'a str>,
    pub doc_comment: Option<&'a str>,
}

/// The cross-encoder text of `node`. `lines` is the node's source file
/// split on newlines (see [`source_lines`]), `None` when it has none; `tok`
/// counts a string's tokens without special tokens.
pub(crate) fn ce_text(node: &CeNode<'_>, lines: Option<&[String]>, tok: &dyn Fn(&str) -> usize) -> String {
    let fields = [
        enclosing(node.kind, node.native_kind, node.name.unwrap_or(""), node.qualified_name),
        path_words(node.file_path),
        text_to_embed(node.doc_comment, node.signature).unwrap_or_default(),
        body(lines, node.start_line, node.end_line, node.kind, node.language, tok),
    ];
    join(&fields, "\n")
}

/// A source file's lines as the text reads them: invalid UTF-8 replaced,
/// `\r\n` and a lone `\r` read as `\n`. `None` when the file cannot be read.
pub(crate) fn source_lines(path: &Path) -> Option<Vec<String>> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n").replace('\r', "\n");
    Some(text.split('\n').map(str::to_string).collect())
}

/// Python's `str.isspace`: Unicode `White_Space` plus the ASCII separators
/// `\x1c`-`\x1f`, which `char::is_whitespace` leaves out.
fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

fn join(parts: &[String], separator: &str) -> String {
    parts.iter().filter(|part| !part.is_empty()).map(String::as_str).collect::<Vec<_>>().join(separator)
}

/// The lowercased ASCII words of `s`, space-separated: `HTTPServer` ->
/// `http server`, `safeInt8` -> `safe int 8`, `send_quit` -> `send quit`.
/// Any other character separates words. The Python's regex
/// `[A-Z]+(?=[A-Z][a-z])|[A-Z]?[a-z]+|[A-Z]+|[0-9]+`, hand-scanned.
pub(crate) fn split_words(s: &str) -> String {
    let b = s.as_bytes();
    let run = |from: usize, class: fn(&u8) -> bool| from + b[from..].iter().take_while(|c| class(c)).count();
    let mut words: Vec<String> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let end = if b[i].is_ascii_uppercase() {
            let upper_end = run(i, u8::is_ascii_uppercase);
            if upper_end - i >= 2 && b.get(upper_end).is_some_and(u8::is_ascii_lowercase) {
                // An acronym before a capitalised word: its last capital
                // starts the next word.
                upper_end - 1
            } else if b.get(i + 1).is_some_and(u8::is_ascii_lowercase) {
                run(i + 1, u8::is_ascii_lowercase)
            } else {
                upper_end
            }
        } else if b[i].is_ascii_lowercase() {
            run(i, u8::is_ascii_lowercase)
        } else if b[i].is_ascii_digit() {
            run(i, u8::is_ascii_digit)
        } else {
            i += 1;
            continue;
        };
        words.push(s[i..end].to_ascii_lowercase());
        i = end;
    }
    words.join(" ")
}

/// Kind words, then the words of `qualified_name` minus its trailing `name`
/// (the whole qualified name when it does not end in `name` or `name` is
/// empty).
pub(crate) fn enclosing(
    kind: Option<&str>,
    native_kind: Option<&str>,
    name: &str,
    qualified_name: Option<&str>,
) -> String {
    let kind = native_kind.filter(|k| !k.is_empty()).or(kind).unwrap_or("");
    let qualified_name = qualified_name.unwrap_or("");
    let prefix = match qualified_name.strip_suffix(name) {
        Some(prefix) if !name.is_empty() => prefix,
        _ => qualified_name,
    };
    join(&[split_words(kind), split_words(prefix)], " ")
}

/// The words of the file path.
pub(crate) fn path_words(file_path: Option<&str>) -> String {
    split_words(file_path.unwrap_or(""))
}

/// Index of the signature's last line, scanning from `start` (0-based): for
/// Python the first line that ends in `:` at bracket depth 0 (past
/// decorators and a multi-line header); for the other languages the first
/// line holding a `{`, `;` or `=>` at bracket depth 0. `start` when no such
/// line exists up to `end`. String contents and line comments are skipped;
/// outside Python only `"` and `` ` `` open a string (`'` is a Rust
/// lifetime).
pub(crate) fn signature_end(lines: &[String], start: usize, end: usize, language: Option<&str>) -> usize {
    let py = language == Some("python");
    let quotes: [char; 2] = if py { ['"', '\''] } else { ['"', '`'] };
    let Some(last) = lines.len().checked_sub(1).map(|last| last.min(end)) else { return start };
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    for (i, line) in lines.iter().enumerate().take(last + 1).skip(start) {
        let chars: Vec<char> = line.chars().collect();
        let mut tail: Option<char> = None;
        let mut j = 0;
        while j < chars.len() {
            let ch = chars[j];
            let next = chars.get(j + 1).copied();
            if let Some(open) = quote {
                if ch == '\\' {
                    j += 1;
                } else if ch == open {
                    quote = None;
                }
            } else if quotes.contains(&ch) {
                quote = Some(ch);
            } else if (py && ch == '#') || (!py && ch == '/' && next == Some('/')) {
                break;
            } else if ch == '(' || ch == '[' {
                depth += 1;
            } else if ch == ')' || ch == ']' {
                depth = depth.saturating_sub(1);
            } else if depth == 0 && !py && (ch == '{' || ch == ';' || (ch == '=' && next == Some('>'))) {
                return i;
            }
            if quote.is_none() && !is_py_space(ch) {
                tail = Some(ch);
            }
            j += 1;
        }
        if py {
            // A Python string does not continue past its line.
            quote = None;
            if depth == 0 && tail == Some(':') {
                return i;
            }
        }
    }
    start
}

/// Index of the first line after a leading Python docstring in `lines`
/// (0 when there is none; past the end when it is not closed).
fn skip_docstring(lines: &[String]) -> usize {
    let Some(i) = lines.iter().position(|line| !line.trim_matches(is_py_space).is_empty()) else { return 0 };
    let head = lines[i].trim_matches(is_py_space);
    let prefix = head.bytes().take_while(|b| b"rRbBuUfF".contains(b)).count();
    if prefix > 2 {
        return 0;
    }
    let Some(quote) = ["\"\"\"", "'''"].into_iter().find(|q| head[prefix..].starts_with(q)) else { return 0 };
    if head[prefix + quote.len()..].contains(quote) {
        return i + 1;
    }
    let close =
        lines[i + 1..].iter().position(|line| line.contains(quote)).map_or(lines.len(), |k| i + 1 + k);
    close + 1
}

/// Whether `line` holds only whitespace and `{}()[];,`.
fn punctuation_only(line: &str) -> bool {
    line.chars().all(|c| is_py_space(c) || "{}()[];,".contains(c))
}

/// Source lines after the signature ([`signature_end`]) up to `end`
/// (0-based, inclusive, clipped to the file), a leading Python docstring
/// skipped, lines of only whitespace and `{}()[];,` dropped, whitespace runs
/// collapsed to one space, each line cut at [`LINE_CHARS`] characters.
pub(crate) fn body_lines(lines: &[String], start: usize, end: usize, language: Option<&str>) -> Vec<String> {
    let from = signature_end(lines, start, end, language).saturating_add(1);
    let to = end.saturating_add(1).min(lines.len());
    let mut span = if from < to { &lines[from..to] } else { &[][..] };
    if language == Some("python") {
        span = &span[skip_docstring(span).min(span.len())..];
    }
    span.iter()
        .filter(|line| !punctuation_only(line))
        .map(|line| {
            let collapsed = line.split(is_py_space).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
            collapsed.chars().take(LINE_CHARS).collect()
        })
        .collect()
}

/// Whole body lines, joined by newlines, while the field stays within
/// [`BODY_BUDGET`] tokens; it stops at the first line that does not fit.
/// Empty for kinds outside [`BODY_KINDS`] and without source lines.
pub(crate) fn body(
    lines: Option<&[String]>,
    start: usize,
    end: usize,
    kind: Option<&str>,
    language: Option<&str>,
    tok: &dyn Fn(&str) -> usize,
) -> String {
    let Some(lines) = lines.filter(|lines| !lines.is_empty()) else { return String::new() };
    if !kind.is_some_and(|kind| BODY_KINDS.contains(&kind)) {
        return String::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut tokens = 0usize;
    for line in body_lines(lines, start, end, language) {
        tokens = tokens.saturating_add(tok(&line));
        if tokens > BODY_BUDGET {
            break;
        }
        out.push(line);
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Fixture {
        body_budget: usize,
        cases: Vec<Case>,
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Case {
        label: String,
        node: FixtureNode,
        /// The source file's bytes (base64), for the cases that pin how a
        /// file is read; `src_lines` is then the Python's reading of them.
        src_bytes: Option<String>,
        src_lines: Option<Vec<String>>,
        words: String,
        /// The text with the body counted by the cross-encoder's tokenizer.
        minilm: String,
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct FixtureNode {
        kind: Option<String>,
        native_kind: Option<String>,
        name: Option<String>,
        qualified_name: Option<String>,
        file_path: Option<String>,
        start_line: usize,
        end_line: usize,
        language: Option<String>,
        signature: Option<String>,
        doc_comment: Option<String>,
    }

    impl FixtureNode {
        fn as_node(&self) -> CeNode<'_> {
            CeNode {
                kind: self.kind.as_deref(),
                native_kind: self.native_kind.as_deref(),
                name: self.name.as_deref(),
                qualified_name: self.qualified_name.as_deref(),
                file_path: self.file_path.as_deref(),
                start_line: self.start_line,
                end_line: self.end_line,
                language: self.language.as_deref(),
                signature: self.signature.as_deref(),
                doc_comment: self.doc_comment.as_deref(),
            }
        }
    }

    /// Python's `len(s.split())`.
    fn whitespace_words(s: &str) -> usize {
        s.split(is_py_space).filter(|w| !w.is_empty()).count()
    }

    /// Every fixture case, built here with a whitespace-word counter,
    /// equals the Python builder's text byte for byte; a case given as file
    /// bytes is read into the same lines as the Python read.
    #[test]
    fn every_fixture_case_equals_the_python_text() {
        use base64::prelude::{Engine, BASE64_STANDARD};

        let fixture: Fixture = serde_json::from_str(include_str!("testdata/ce_text.json")).unwrap();
        assert_eq!(fixture.body_budget, BODY_BUDGET);
        let dir = tempfile::tempdir().unwrap();
        let mut mismatches = Vec::new();
        for case in &fixture.cases {
            if let Some(encoded) = &case.src_bytes {
                let path = dir.path().join("src");
                std::fs::write(&path, BASE64_STANDARD.decode(encoded).unwrap()).unwrap();
                let lines = source_lines(&path);
                if lines != case.src_lines {
                    mismatches.push(format!("{}: read {lines:?}, python {:?}", case.label, case.src_lines));
                }
            }
            let text = ce_text(&case.node.as_node(), case.src_lines.as_deref(), &whitespace_words);
            if text != case.words {
                mismatches.push(format!("{}:\n  ours   {text:?}\n  python {:?}", case.label, case.words));
            }
        }
        assert!(
            mismatches.is_empty(),
            "{} of {} cases differ:\n{}",
            mismatches.len(),
            fixture.cases.len(),
            mismatches.join("\n")
        );
    }

    /// Every fixture case, built with the cross-encoder's own token count,
    /// equals the Python builder's text under the same tokenizer (the
    /// `minilm` field, which differs from `words` where the body stops).
    /// Needs the real model: `G_MESH_RERANK_MODEL_DIR=<dir> cargo test rerank
    /// -- --ignored`.
    #[test]
    #[ignore = "needs the real cross-encoder; see rerank.rs's tests"]
    fn every_fixture_case_equals_the_python_text_under_the_rerank_tokenizer() {
        use crate::embedding::rerank::{default_rerank_model_dir, CrossEncoder, Scorer};

        let dir = default_rerank_model_dir().unwrap();
        let model = CrossEncoder::load(&dir)
            .unwrap_or_else(|err| panic!("could not load the cross-encoder: {err:#}"));
        let fixture: Fixture = serde_json::from_str(include_str!("testdata/ce_text.json")).unwrap();
        let differ_from_words = fixture.cases.iter().filter(|case| case.minilm != case.words).count();
        assert!(differ_from_words > 0, "no case tells the tokenizer from a word count");
        let mismatches: Vec<String> = fixture
            .cases
            .iter()
            .filter_map(|case| {
                let text = ce_text(&case.node.as_node(), case.src_lines.as_deref(), &|text| {
                    model.count_tokens(text)
                });
                (text != case.minilm)
                    .then(|| format!("{}:\n  ours   {text:?}\n  python {:?}", case.label, case.minilm))
            })
            .collect();
        assert!(
            mismatches.is_empty(),
            "{} of {} cases differ:\n{}",
            mismatches.len(),
            fixture.cases.len(),
            mismatches.join("\n")
        );
    }
}
