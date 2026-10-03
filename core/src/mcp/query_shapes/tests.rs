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
        language: "typescript".to_string(),
        non_symbol_queries: shapes(&["@"], &[]),
        ..crate::daemon::plugin::bundled_manifest()
    };
    let silent =
        PluginManifest { language: "cobol".to_string(), ..crate::daemon::plugin::bundled_manifest() };

    let map = QueryShapes::from_manifests([&declared, &silent]);

    assert!(map.refuses("typescript", "@x"));
    assert!(!map.refused_by_all("@x"));
}

/// The committed manifests declare what the design table says: every shipped
/// plugin refuses a leading `@` and any `/`, and nothing else.
///
/// Control: delete the `[plugin.non_symbol_queries]` table from any one
/// shipped `plugin.toml` - its entry comes back empty and this fails.
#[test]
fn the_shipped_manifests_declare_at_and_slash() {
    for language in ["typescript", "go", "rust", "python"] {
        assert_eq!(QueryShapes::shipped().get(language), Some(&shapes(&["@"], &["/"])), "{language}");
    }
}
