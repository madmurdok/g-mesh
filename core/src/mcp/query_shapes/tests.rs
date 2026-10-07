use super::*;

fn shapes(starts_with: &[&str], contains: &[&str]) -> NonSymbolShapes {
    NonSymbolShapes {
        starts_with: starts_with.iter().map(|s| s.to_string()).collect(),
        contains: contains.iter().map(|s| s.to_string()).collect(),
    }
}

/// Control: make `refuses` return `self.refused_by_all(query)` - Go's `@x`
/// below is then not refused for TypeScript-only shapes, and this fails.
#[test]
fn refuses_answers_for_the_named_language_only() {
    let map = QueryShapes::of(&[("typescript", shapes(&["@"], &[])), ("go", shapes(&[], &[]))]);

    assert!(map.refuses("typescript", "@x"));
    assert!(!map.refuses("go", "@x"), "go declares nothing");
    assert!(!map.refuses("cobol", "@x"), "a language with no entry refuses nothing");
    assert!(!map.refuses("typescript", "x@"), "starts_with is a prefix, not an infix");
}

/// Control: drop `!self.0.is_empty() &&` from `refused_by_all` - the empty
/// map then refuses everything and this fails.
#[test]
fn refused_by_all_needs_every_language_and_at_least_one() {
    let both = QueryShapes::of(&[("typescript", shapes(&["@"], &["/"])), ("go", shapes(&["@"], &["/"]))]);
    let one = QueryShapes::of(&[("typescript", shapes(&["@"], &["/"])), ("go", shapes(&[], &["/"]))]);

    assert!(both.refused_by_all("@scope/pkg"));
    assert!(both.refused_by_all("a/b"));
    assert!(!both.refused_by_all("Component"));
    assert!(one.refused_by_all("a/b"));
    assert!(!one.refused_by_all("@Component"), "go does not refuse `@`");
    assert!(!QueryShapes::default().refused_by_all("@scope/pkg"), "no languages refuse nothing");
}

/// A discovered plugin without the table is still a language that does not
/// refuse, so it keeps the rung open for its own candidates.
///
/// Control: in `from_manifests`, skip manifests whose shapes are empty -
/// `refused_by_all` then holds for `@x` and this fails.
#[test]
fn a_plugin_without_the_table_keeps_refused_by_all_false() {
    let declared = PluginManifest {
        non_symbol_queries: shapes(&["@"], &[]),
        ..crate::daemon::manifest::bare_manifest("typescript")
    };
    let silent = crate::daemon::manifest::bare_manifest("cobol");

    let map = QueryShapes::from_manifests([&declared, &silent]);

    assert!(map.refuses("typescript", "@x"));
    assert!(!map.refused_by_all("@x"));
}

/// The committed manifests declare what the design table says: every shipped
/// plugin refuses a leading `@` and any `/`, TypeScript also a leading
/// `node:` (a Node.js built-in specifier), Python also a leading `.` (a
/// relative import), and nothing else.
///
/// Control: delete the `[plugin.non_symbol_queries]` table from any one
/// shipped `plugin.toml`, the `"node:"` from TypeScript's, or the `"."` from
/// Python's - its entry comes back different and this fails.
#[test]
fn the_shipped_manifests_declare_at_and_slash() {
    for language in ["go", "rust"] {
        assert_eq!(QueryShapes::shipped().get(language), Some(&shapes(&["@"], &["/"])), "{language}");
    }
    assert_eq!(QueryShapes::shipped().get("typescript"), Some(&shapes(&["@", "node:"], &["/"])));
    assert_eq!(QueryShapes::shipped().get("python"), Some(&shapes(&["@", "."], &["/"])));
}

/// A Node.js built-in specifier is never a TypeScript symbol, and the prefix
/// is literal: an identifier that merely starts with `node` is not refused.
///
/// Control: remove `"node:"` from `starts_with` in
/// `plugins/typescript/plugin.toml` - `node:fs` is then accepted and this
/// fails.
#[test]
fn typescript_refuses_node_builtin_specifiers_only() {
    let shipped = QueryShapes::shipped();
    for query in ["node:fs", "node:path", "node:child_process", "node:test"] {
        assert!(shipped.refuses("typescript", query), "{query}");
        assert!(!shipped.refuses("go", query), "{query}: go declares no `node:`");
    }
    for query in ["node", "nodeFs", "NodePath", "nodes"] {
        assert!(!shipped.refuses("typescript", query), "{query}");
    }
}

/// T5: the pairs to retry. Only a language that strips the prefix gets one;
/// a remainder that is empty, or that the same language refuses as typed,
/// gets none - so `@` is stripped at most once and a scoped package or a
/// path is never looked up.
///
/// Control: drop `&& !shapes.refused.matches(remainder)` from `rewrites` -
/// `@scope/pkg` gives `[("typescript", "scope/pkg")]` and this fails.
#[test]
fn rewrites_strip_a_declared_prefix_and_keep_only_a_remainder_the_language_accepts() {
    let map = QueryShapes::of(&[("typescript", shapes(&["@"], &["/"])), ("go", shapes(&["@"], &["/"]))])
        .with_strip("typescript", &["@"]);

    assert_eq!(map.rewrites("@Component"), vec![("typescript", "Component")]);
    for query in ["@", "@@Component", "@scope/pkg", "@src/app.ts", "Component", "x@Component"] {
        assert_eq!(map.rewrites(query), Vec::<(&str, &str)>::new(), "{query}");
    }
    let both = map.with_strip("go", &["@"]);
    assert_eq!(both.rewrites("@Component"), vec![("go", "Component"), ("typescript", "Component")]);
    assert_eq!(QueryShapes::default().rewrites("@Component"), Vec::<(&str, &str)>::new());
}

/// A discovered plugin without the table rewrites nothing for its language.
///
/// Control: in `from_manifests`, take `strip` from any manifest that declares
/// one for every language - `cobol` gets a pair and this fails.
#[test]
fn a_plugin_without_the_table_is_never_rewritten_for() {
    let declared = PluginManifest {
        non_symbol_queries: shapes(&["@"], &[]),
        symbol_query_prefixes: crate::daemon::manifest::SymbolQueryPrefixes { strip: vec!["@".to_string()] },
        ..crate::daemon::manifest::bare_manifest("typescript")
    };
    let silent = PluginManifest {
        non_symbol_queries: shapes(&["@"], &[]),
        ..crate::daemon::manifest::bare_manifest("cobol")
    };

    let map = QueryShapes::from_manifests([&declared, &silent]);

    assert_eq!(map.rewrites("@Component"), vec![("typescript", "Component")]);
}

/// T6: the committed manifests opt in as ADR 0019 says: TypeScript and
/// Python strip `@`, Rust and Go strip nothing.
///
/// Control: none needed beyond the manifests themselves - delete the
/// `[plugin.symbol_query_prefixes]` table from TypeScript's or Python's
/// `plugin.toml`, or add one to Go's or Rust's, and this fails.
#[test]
fn the_shipped_manifests_strip_at_for_typescript_and_python_only() {
    let shipped = QueryShapes::shipped();
    for language in ["typescript", "python"] {
        assert_eq!(shipped.strip(language), Some(&["@".to_string()][..]), "{language}");
    }
    for language in ["go", "rust"] {
        assert_eq!(shipped.strip(language), Some(&[][..]), "{language}");
    }
}
