//! A plugin's [`QualifiedPath`] in storage: the column encoding of
//! `nodes.qualifiedPath`/`placeholder_targets.keyPath`, and the
//! `qualified_suffixes` rows derived from a declaration's paths.
//!
//! Only joins what the plugin sent. Nothing here splits a display string or
//! knows any language's separators. Design:
//! [ADR 0015](../../../docs/adr/0015-qualified-name-segments.md).

use std::collections::BTreeSet;

use crate::protocol::types::{PathSegment, QualifiedPath};

/// Joins the encoded elements. No segment contains it
/// (`protocol::types::PATH_FORBIDDEN_CHARS`), which ingest enforces.
const JOINER: char = '\u{1f}';

/// `name0 US sep1 US name1 ...`.
pub fn encode(path: &QualifiedPath) -> String {
    let mut text = String::new();
    for (i, segment) in path.segments().iter().enumerate() {
        if i > 0 {
            text.push(JOINER);
            text.push_str(segment.sep.as_deref().unwrap_or(""));
            text.push(JOINER);
        }
        text.push_str(&segment.name);
    }
    text
}

/// The inverse of [`encode`]; `None` for text [`encode`] cannot produce.
pub fn decode(text: &str) -> Option<QualifiedPath> {
    let mut parts = text.split(JOINER);
    let mut segments = vec![PathSegment { sep: None, name: parts.next()?.to_string() }];
    while let Some(sep) = parts.next() {
        let name = parts.next()?;
        segments.push(PathSegment { sep: Some(sep.to_string()), name: name.to_string() });
    }
    Some(QualifiedPath(segments))
}

/// The `qualified_suffixes` rows of a declaration: the suffixes of `path`
/// starting at segment 1 or later and of each alias starting at segment 0 or
/// later, each keeping at least two segments, as display text. Deduplicated.
pub fn suffixes(path: Option<&QualifiedPath>, aliases: &[QualifiedPath]) -> BTreeSet<String> {
    let mut rows = BTreeSet::new();
    let mut add = |path: &QualifiedPath, first: usize| {
        for start in first..path.len().saturating_sub(1) {
            rows.insert(path.suffix_from(start));
        }
    };
    if let Some(path) = path {
        add(path, 1);
    }
    for alias in aliases {
        add(alias, 0);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(first: &str, rest: &[(&str, &str)]) -> QualifiedPath {
        rest.iter().fold(QualifiedPath::root(first), |path, (sep, name)| path.child(*sep, *name))
    }

    #[test]
    fn encoding_round_trips_and_keeps_each_separator() {
        let original = path("a", &[("::", "T"), (".", "f")]);
        let encoded = encode(&original);
        assert_eq!(encoded, "a\u{1f}::\u{1f}T\u{1f}.\u{1f}f");
        assert_eq!(decode(&encoded), Some(original));
        assert_eq!(decode("solo"), Some(QualifiedPath::root("solo")));
        assert_eq!(decode("a\u{1f}::"), None, "a separator with no name after it");
    }

    #[test]
    fn suffixes_skip_the_whole_primary_path_but_keep_whole_aliases() {
        let primary =
            path("storage", &[("::", "index_store"), ("::", "<IndexStore as Read>"), ("::", "read")]);
        let alias = path("storage", &[("::", "index_store"), ("::", "IndexStore"), ("::", "read")]);
        let rows: Vec<String> = suffixes(Some(&primary), &[alias]).into_iter().collect();
        assert_eq!(
            rows,
            vec![
                "<IndexStore as Read>::read",
                "IndexStore::read",
                "index_store::<IndexStore as Read>::read",
                "index_store::IndexStore::read",
                "storage::index_store::IndexStore::read",
            ]
        );
    }

    #[test]
    fn a_path_of_two_segments_or_fewer_has_no_suffix_rows_of_its_own() {
        assert!(suffixes(Some(&path("C", &[("#", "m")])), &[]).is_empty());
        assert!(suffixes(Some(&QualifiedPath::root("f")), &[]).is_empty());
        assert!(suffixes(None, &[]).is_empty());
    }
}
