//! The language catalogue: what core knows about a language *without* its
//! plugin.
//!
//! Discovery (`daemon::manifest::discover`) only knows the languages whose
//! plugin is installed, and the index only holds files some plugin claimed.
//! So without this table core cannot even name a language it has no plugin
//! for, let alone tell a person how to get one. The catalogue ships with core,
//! not with any plugin, so it is there exactly when the plugin is not.
//!
//! # Deliberately thin
//!
//! An entry holds a language id, the file extensions that language's plugin
//! claims, and (derived from the id) the command that installs that plugin.
//! Nothing else. In particular it holds **no capability fields**: capabilities
//! belong to the plugin manifest alone (`daemon::manifest::Capabilities`).
//! The moment the catalogue holds a capability, someone reads it from here
//! instead of from the manifest, and core has two sources of truth about a
//! live plugin that will drift apart. The catalogue describes only a plugin
//! that is *absent*; a present one describes itself.
//!
//! # Precedence
//!
//! A discovered manifest always wins. The catalogue is consulted only for a
//! language with no discovered manifest ([`missing`]), and an extension lookup
//! ([`absent_for_path`]) defers to discovery whenever a manifest claims that
//! extension or the catalogue's language for it.
//!
//! # Adding a language
//!
//! One entry in [`CATALOGUE`]; nothing else in core is per-language. Take the
//! extensions from that plugin's own `plugins/<language>/plugin.toml`
//! (`[plugin.languages] extensions`), lowercase with a leading dot, exactly as
//! the manifest spells them.

use crate::daemon::manifest::{extension_of, DiscoveredPlugins};

/// One catalogued language: what core may say about it while its plugin is
/// absent. See the module doc for why there is nothing more here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogueEntry {
    /// The language id, equal to its plugin manifest's `language` (and so to
    /// the plugin's directory name under `plugins/`).
    pub language: &'static str,
    /// Lowercase, leading-dot extensions, copied from the plugin's
    /// `plugin.toml` `[plugin.languages] extensions`.
    pub extensions: &'static [&'static str],
}

impl CatalogueEntry {
    /// The exact command that installs this language's plugin:
    /// `g-mesh plugins install <language>`.
    ///
    /// That subcommand does not exist yet (GM-331 adds it, in the same
    /// release); nothing runs or prints this today. It is derived from the
    /// language id rather than stored, so an entry cannot name one language
    /// and install another.
    pub fn install_command(&self) -> String {
        format!("g-mesh plugins install {}", self.language)
    }
}

/// Every catalogued language, in a fixed order. Adding a language is adding
/// one entry here.
pub const CATALOGUE: &[CatalogueEntry] = &[
    CatalogueEntry {
        language: "typescript",
        extensions: &[".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"],
    },
    CatalogueEntry { language: "python", extensions: &[".py", ".pyi"] },
    CatalogueEntry { language: "rust", extensions: &[".rs"] },
    CatalogueEntry { language: "go", extensions: &[".go"] },
];

/// The catalogue entry for `language`, whether or not its plugin is present.
pub fn entry(language: &str) -> Option<&'static CatalogueEntry> {
    entry_in(CATALOGUE, language)
}

/// The catalogue entry claiming `file_path`'s extension (case-insensitive,
/// as discovery's routing is), whether or not its plugin is present.
pub fn entry_for_path(file_path: &str) -> Option<&'static CatalogueEntry> {
    entry_for_path_in(CATALOGUE, file_path)
}

/// The precedence rule, for a whole set: every catalogued language with
/// **no** discovered manifest, in catalogue order. A language discovery found
/// is never returned, whatever its manifest says, because the manifest is the
/// only source of truth about a present plugin.
pub fn missing(discovered: &DiscoveredPlugins) -> Vec<&'static CatalogueEntry> {
    missing_in(CATALOGUE, discovered)
}

/// The precedence rule, for one file: the catalogue entry naming the absent
/// plugin that would index `file_path`, or `None` when discovery already
/// answers for it. `None` when:
/// - a discovered manifest claims the file's extension (discovery routes it,
///   whatever the catalogue says that extension belongs to);
/// - the catalogue's language for that extension has a discovered manifest
///   (the plugin is present, even if its manifest no longer claims this
///   extension - the manifest wins, so the file is simply not routed);
/// - no catalogue entry claims the extension, or the path has none.
pub fn absent_for_path(discovered: &DiscoveredPlugins, file_path: &str) -> Option<&'static CatalogueEntry> {
    absent_for_path_in(CATALOGUE, discovered, file_path)
}

// The lookups are written over any table, and the public functions above
// pass `CATALOGUE`: no lookup may depend on which languages the table holds,
// so that a new language is one entry and nothing else.

fn entry_in<'a>(table: &'a [CatalogueEntry], language: &str) -> Option<&'a CatalogueEntry> {
    table.iter().find(|entry| entry.language == language)
}

fn entry_for_path_in<'a>(table: &'a [CatalogueEntry], file_path: &str) -> Option<&'a CatalogueEntry> {
    let extension = extension_of(file_path)?;
    table.iter().find(|entry| entry.extensions.contains(&extension.as_str()))
}

fn missing_in<'a>(table: &'a [CatalogueEntry], discovered: &DiscoveredPlugins) -> Vec<&'a CatalogueEntry> {
    table.iter().filter(|entry| !discovered.manifests.contains_key(entry.language)).collect()
}

fn absent_for_path_in<'a>(
    table: &'a [CatalogueEntry],
    discovered: &DiscoveredPlugins,
    file_path: &str,
) -> Option<&'a CatalogueEntry> {
    if discovered.language_for(file_path).is_some() {
        return None;
    }
    let entry = entry_for_path_in(table, file_path)?;
    if discovered.manifests.contains_key(entry.language) {
        return None;
    }
    Some(entry)
}

#[cfg(test)]
mod tests;
