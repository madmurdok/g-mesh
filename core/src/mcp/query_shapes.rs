//! Which query shapes each discovered language declares are never its
//! symbols (`[plugin.non_symbol_queries]`, `daemon::manifest::NonSymbolShapes`),
//! keyed by language. Core holds no such shape of its own: a language's
//! shapes only ever set aside that language's candidates. Decision:
//! `docs/adr/0018-non-symbol-query-shapes.md`.

use std::collections::HashMap;

use crate::daemon::manifest::{NonSymbolShapes, PluginManifest};

/// Every discovered language, with its declared shapes (empty when its
/// manifest has no table). Built once per daemon from the discovered
/// manifests, which never change while it runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct QueryShapes(HashMap<String, NonSymbolShapes>);

impl QueryShapes {
    pub(crate) fn from_manifests<'a>(manifests: impl IntoIterator<Item = &'a PluginManifest>) -> Self {
        Self(
            manifests
                .into_iter()
                .map(|manifest| (manifest.language.clone(), manifest.non_symbol_queries.clone()))
                .collect(),
        )
    }

    /// Whether `query` is never a symbol of `language`. A language with no
    /// entry refuses nothing.
    pub(crate) fn refuses(&self, language: &str, query: &str) -> bool {
        self.0.get(language).is_some_and(|shapes| shapes.matches(query))
    }

    /// Whether every discovered language refuses `query`, so no candidate of
    /// any language could survive [`refuses`](Self::refuses). False with no
    /// languages at all: nothing is refused without a declaration.
    pub(crate) fn refused_by_all(&self, query: &str) -> bool {
        !self.0.is_empty() && self.0.values().all(|shapes| shapes.matches(query))
    }

    /// The four shipped plugins' declarations, read from their committed
    /// manifests, for tests that exercise the semantic rung without a
    /// registry.
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
                        let shapes = crate::daemon::manifest::non_symbol_queries_of(contents)
                            .unwrap_or_else(|e| panic!("the shipped {language} manifest: {e:#}"));
                        (language.to_string(), shapes)
                    })
                    .collect(),
            )
        });
        &SHIPPED
    }

    #[cfg(test)]
    pub(crate) fn of(entries: &[(&str, NonSymbolShapes)]) -> Self {
        Self(entries.iter().map(|(language, shapes)| (language.to_string(), shapes.clone())).collect())
    }

    #[cfg(test)]
    pub(crate) fn get(&self, language: &str) -> Option<&NonSymbolShapes> {
        self.0.get(language)
    }
}

#[cfg(test)]
mod tests;
