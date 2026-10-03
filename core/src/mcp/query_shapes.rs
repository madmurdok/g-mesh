//! Which query shapes each discovered language declares are never its
//! symbols (`[plugin.non_symbol_queries]`, `daemon::manifest::NonSymbolShapes`),
//! and which prefixes, stripped, can leave one of its symbols
//! (`[plugin.symbol_query_prefixes]`), keyed by language. Core holds no such
//! shape or prefix of its own: a language's declarations only ever set aside,
//! or retry, that language's candidates. Decisions:
//! `docs/adr/0018-non-symbol-query-shapes.md`,
//! `docs/adr/0019-symbol-query-prefixes.md`.

use std::collections::HashMap;

use crate::daemon::manifest::{NonSymbolShapes, PluginManifest};

/// One language's two declarations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LanguageShapes {
    refused: NonSymbolShapes,
    strip: Vec<String>,
}

/// Every discovered language, with its declared shapes and strip prefixes
/// (empty when its manifest has no table). Built once per daemon from the
/// discovered manifests, which never change while it runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct QueryShapes(HashMap<String, LanguageShapes>);

impl QueryShapes {
    pub(crate) fn from_manifests<'a>(manifests: impl IntoIterator<Item = &'a PluginManifest>) -> Self {
        Self(
            manifests
                .into_iter()
                .map(|manifest| {
                    let shapes = LanguageShapes {
                        refused: manifest.non_symbol_queries.clone(),
                        strip: manifest.symbol_query_prefixes.strip.clone(),
                    };
                    (manifest.language.clone(), shapes)
                })
                .collect(),
        )
    }

    /// Whether `query` is never a symbol of `language`. A language with no
    /// entry refuses nothing.
    pub(crate) fn refuses(&self, language: &str, query: &str) -> bool {
        self.0.get(language).is_some_and(|shapes| shapes.refused.matches(query))
    }

    /// Whether every discovered language refuses `query`, so no candidate of
    /// any language could survive [`refuses`](Self::refuses). False with no
    /// languages at all: nothing is refused without a declaration.
    pub(crate) fn refused_by_all(&self, query: &str) -> bool {
        !self.0.is_empty() && self.0.values().all(|shapes| shapes.refused.matches(query))
    }

    /// The `(language, remainder)` pairs to retry after `query` itself
    /// matched nothing: for each language one of whose strip prefixes `query`
    /// starts with, `query` without it. A pair is left out when the remainder
    /// is empty or is refused by that language's own shapes, which is how a
    /// prefix is stripped at most once and a specifier or path never becomes
    /// a lookup. Sorted by language, so the order never depends on the map.
    pub(crate) fn rewrites<'s, 'q>(&'s self, query: &'q str) -> Vec<(&'s str, &'q str)> {
        let mut pairs: Vec<(&str, &str)> = self
            .0
            .iter()
            .filter_map(|(language, shapes)| {
                let remainder = shapes.strip.iter().find_map(|prefix| query.strip_prefix(prefix.as_str()))?;
                (!remainder.is_empty() && !shapes.refused.matches(remainder))
                    .then_some((language.as_str(), remainder))
            })
            .collect();
        pairs.sort_unstable();
        pairs
    }

    /// The four shipped plugins' declarations, read from their committed
    /// manifests, for tests that exercise the ladder without a registry.
    #[cfg(test)]
    pub(crate) fn shipped() -> &'static QueryShapes {
        static SHIPPED: std::sync::LazyLock<QueryShapes> = std::sync::LazyLock::new(|| {
            let manifests = [
                ("typescript", include_str!("../../../plugins/typescript/plugin.toml")),
                ("go", include_str!("../../../plugins/go/plugin.toml")),
                ("rust", include_str!("../../../plugins/rust/plugin.toml")),
                ("python", include_str!("../../../plugins/python/plugin.toml")),
            ];
            QueryShapes(
                manifests
                    .into_iter()
                    .map(|(language, contents)| {
                        let (refused, prefixes) = crate::daemon::manifest::query_tables_of(contents)
                            .unwrap_or_else(|e| panic!("the shipped {language} manifest: {e:#}"));
                        (language.to_string(), LanguageShapes { refused, strip: prefixes.strip })
                    })
                    .collect(),
            )
        });
        &SHIPPED
    }

    #[cfg(test)]
    pub(crate) fn of(entries: &[(&str, NonSymbolShapes)]) -> Self {
        Self(
            entries
                .iter()
                .map(|(language, shapes)| {
                    (language.to_string(), LanguageShapes { refused: shapes.clone(), strip: Vec::new() })
                })
                .collect(),
        )
    }

    /// This map with `language`'s strip prefixes set to `strip`.
    #[cfg(test)]
    pub(crate) fn with_strip(mut self, language: &str, strip: &[&str]) -> Self {
        self.0.entry(language.to_string()).or_default().strip = strip.iter().map(|s| s.to_string()).collect();
        self
    }

    #[cfg(test)]
    pub(crate) fn get(&self, language: &str) -> Option<&NonSymbolShapes> {
        self.0.get(language).map(|shapes| &shapes.refused)
    }

    #[cfg(test)]
    pub(crate) fn strip(&self, language: &str) -> Option<&[String]> {
        self.0.get(language).map(|shapes| shapes.strip.as_slice())
    }
}

#[cfg(test)]
mod tests;
