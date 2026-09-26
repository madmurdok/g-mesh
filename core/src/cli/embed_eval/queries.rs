//! The query files (D3 of `docs/architecture/embedding-eval.md`): loading,
//! the fit/held-out split, and the lexical-overlap stratum.
//!
//! Authored queries live in `queries/<corpus>.jsonl`; the mechanical name
//! queries that enlarge the floor fit set (D6) in
//! `queries/mechanical/<corpus>.jsonl`, and always count as fit.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpectedSymbol {
    pub file_path: String,
    pub qualified_name: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueryKind {
    Positive,
    Absent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Query {
    pub id: String,
    pub corpus: String,
    pub language: String,
    pub kind: QueryKind,
    /// `phrase` or `sentence` for authored queries, `name` for mechanical.
    pub shape: String,
    pub text: String,
    pub expected: Vec<ExpectedSymbol>,
    pub derivation: String,
    pub author: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mechanical: bool,
}

impl Query {
    pub fn positive(&self) -> bool {
        self.kind == QueryKind::Positive
    }

    /// D6 split: held out when the first byte of `sha256(id)` is odd.
    /// Mechanical queries are always fit.
    pub fn held_out(&self) -> bool {
        !self.mechanical && is_held_out_id(&self.id)
    }

    /// D3 step 6.
    pub fn overlap(&self) -> bool {
        overlaps(&self.text, &self.expected)
    }
}

pub fn is_held_out_id(id: &str) -> bool {
    Sha256::digest(id.as_bytes())[0] % 2 == 1
}

/// The queries of `corpus`: authored first, in file order, then mechanical.
/// Returns them with the sha256 of each file read, keyed by its path under
/// the eval dir, which the run manifest records (D3 step 7: the frozen
/// files' hashes).
pub fn load(eval_dir: &Path, corpus: &str) -> Result<(Vec<Query>, Vec<(String, String)>)> {
    let mut queries = Vec::new();
    let mut hashes = Vec::new();
    let authored = eval_dir.join("queries").join(format!("{corpus}.jsonl"));
    let mechanical = eval_dir.join("queries").join("mechanical").join(format!("{corpus}.jsonl"));
    let names = [format!("queries/{corpus}.jsonl"), format!("queries/mechanical/{corpus}.jsonl")];
    for ((path, is_mechanical, required), name) in
        [(&authored, false, true), (&mechanical, true, false)].into_iter().zip(names)
    {
        if !path.exists() {
            if required {
                bail!("query file {} not found", path.display());
            }
            continue;
        }
        let bytes = std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        hashes.push((name, hex(&Sha256::digest(&bytes))));
        let text = String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))?;
        for (lineno, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let mut query: Query = serde_json::from_str(line)
                .with_context(|| format!("{}:{}: not a query record", path.display(), lineno + 1))?;
            if is_mechanical {
                query.mechanical = true;
            }
            validate(&query, corpus).with_context(|| format!("{}:{}", path.display(), lineno + 1))?;
            queries.push(query);
        }
    }
    let mut ids = BTreeSet::new();
    for q in &queries {
        if !ids.insert(q.id.as_str()) {
            bail!("query id {} appears twice in corpus {corpus}", q.id);
        }
    }
    Ok((queries, hashes))
}

fn validate(q: &Query, corpus: &str) -> Result<()> {
    if q.corpus != corpus {
        bail!("query {} belongs to corpus {}, read as {corpus}", q.id, q.corpus);
    }
    match (&q.kind, q.expected.len()) {
        (QueryKind::Positive, 0) => bail!("positive query {} has no expected symbol", q.id),
        (QueryKind::Positive, n) if n > 3 && !q.mechanical => {
            bail!("positive query {} lists {n} expected symbols; D3 allows 1-3", q.id)
        }
        (QueryKind::Absent, n) if n > 0 => bail!("absent query {} lists expected symbols", q.id),
        _ => {}
    }
    let shape_ok = if q.mechanical { q.shape == "name" } else { q.shape == "phrase" || q.shape == "sentence" };
    if !shape_ok {
        bail!("query {} has shape {:?}", q.id, q.shape);
    }
    if q.text.trim().is_empty() {
        bail!("query {} has no text", q.id);
    }
    Ok(())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Last segment of a qualified name, whatever the language's separator
/// (`a::b::c`, `A.b`, `A#b`, `path/to/file.rs`).
pub fn symbol_name(qualified_name: &str) -> &str {
    qualified_name.rsplit(|c| c == ':' || c == '.' || c == '#' || c == '/').next().unwrap_or(qualified_name)
}

/// Camel/snake sub-tokens of an identifier, lower-cased, length >= 4.
pub fn sub_tokens(identifier: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for part in identifier.split(|c: char| !c.is_ascii_alphanumeric()) {
        let chars: Vec<char> = part.chars().collect();
        let mut start = 0;
        for i in 1..=chars.len() {
            let boundary = i == chars.len() || {
                let (prev, cur) = (chars[i - 1], chars[i]);
                let next_lower = chars.get(i + 1).is_some_and(|c| c.is_ascii_lowercase());
                (prev.is_ascii_lowercase() && cur.is_ascii_uppercase())
                    || (prev.is_ascii_uppercase() && cur.is_ascii_uppercase() && next_lower)
                    || (prev.is_ascii_alphabetic() != cur.is_ascii_alphabetic())
            };
            if boundary {
                let piece: String = chars[start..i].iter().collect();
                if piece.len() >= 4 {
                    out.insert(piece.to_ascii_lowercase());
                }
                start = i;
            }
        }
    }
    out
}

/// D3 step 6: the query shares a sub-token with an expected symbol's name.
pub fn overlaps(text: &str, expected: &[ExpectedSymbol]) -> bool {
    let words: BTreeSet<String> = text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    expected
        .iter()
        .any(|e| sub_tokens(symbol_name(&e.qualified_name)).iter().any(|t| words.contains(t)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected(qualified_name: &str) -> ExpectedSymbol {
        ExpectedSymbol { file_path: "f".into(), qualified_name: qualified_name.into(), kind: "Function".into() }
    }

    /// Control: a splitter that does not break on case (or on `_`) fails the
    /// first two; dropping the length filter fails the third.
    #[test]
    fn sub_tokens_split_camel_snake_and_acronyms() {
        let t = sub_tokens("getEncodingFromHeaders");
        assert_eq!(t, ["encoding", "from", "headers"].iter().map(|s| s.to_string()).collect());
        let t = sub_tokens("unquote_header_value");
        assert_eq!(t, ["header", "unquote", "value"].iter().map(|s| s.to_string()).collect());
        let t = sub_tokens("HTTPServerIO");
        assert_eq!(t, ["http", "server"].iter().map(|s| s.to_string()).collect());
    }

    #[test]
    fn the_symbol_name_is_the_last_segment_of_any_separator() {
        assert_eq!(symbol_name("flags::defs::<Engine as Flag>::update"), "update");
        assert_eq!(symbol_name("Context.GetInt8Slice"), "GetInt8Slice");
        assert_eq!(symbol_name("Scene#getFramesIncludingDeleted"), "getFramesIncludingDeleted");
        assert_eq!(symbol_name("plain"), "plain");
    }

    /// Control: matching substrings instead of whole words makes "headers"
    /// in "subheaders" count; matching on the whole qualified name instead
    /// of the last segment makes the module name `utils` count.
    #[test]
    fn overlap_needs_a_whole_word_equal_to_a_name_sub_token() {
        let e = [expected("utils.get_encoding_from_headers")];
        assert!(overlaps("find the Encoding declared in response headers", &e));
        assert!(!overlaps("charset declared in subheaders", &e));
        assert!(!overlaps("helper utils for charsets", &e));
    }

    /// The split is a fixed function of the id: stable, and not all one way.
    /// Control: hashing the text instead of the id, or taking the parity of
    /// the last byte, changes which of these fixed ids are held out.
    #[test]
    fn the_split_is_the_parity_of_the_first_hash_byte() {
        // First bytes computed from sha256 of each id at authoring time are
        // not hard-coded here; instead the rule is checked against the digest.
        for id in ["rg-001", "gin-042", "req-a07", "ttm-013"] {
            let first = Sha256::digest(id.as_bytes())[0];
            assert_eq!(is_held_out_id(id), first % 2 == 1, "{id}");
        }
        let held: usize = (0..200).filter(|i| is_held_out_id(&format!("q-{i:03}"))).count();
        assert!((60..=140).contains(&held), "{held} of 200 held out");
    }

    #[test]
    fn a_mechanical_query_is_never_held_out() {
        let mut q = Query {
            id: String::new(),
            corpus: "c".into(),
            language: "go".into(),
            kind: QueryKind::Positive,
            shape: "name".into(),
            text: "x".into(),
            expected: vec![expected("x")],
            derivation: String::new(),
            author: String::new(),
            mechanical: true,
        };
        // Find an id the hash would hold out, then check the flag wins.
        q.id = (0..).map(|i| format!("m-{i}")).find(|id| is_held_out_id(id)).unwrap();
        assert!(!q.held_out());
        q.mechanical = false;
        assert!(q.held_out());
    }

    #[test]
    fn records_parse_in_the_d3_format() {
        let line = r#"{"id": "rg-017", "corpus": "ripgrep", "language": "rust",
 "kind": "positive", "shape": "sentence",
 "text": "decide whether a path should be skipped because an ignore file excludes it",
 "expected": [{"filePath": "crates/ignore/src/dir.rs", "qualifiedName": "dir::Ignore::matched", "kind": "Function"}],
 "derivation": "Target sampled (seed 398, #17).", "author": "GM-398/S2 agent"}"#;
        let q: Query = serde_json::from_str(&line.replace('\n', "")).unwrap();
        assert!(q.positive());
        assert!(!q.mechanical);
        validate(&q, "ripgrep").unwrap();
        assert!(validate(&q, "gin").is_err());
    }
}
