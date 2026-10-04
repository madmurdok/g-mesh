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
//! claims, the directories that plugin's walk skips, and (derived from the id)
//! the command that installs that plugin. Extensions and skipped directories
//! together say which files the absent plugin *would* index, so core can count
//! them ([`count_absent_files`]); neither says what the plugin can do.
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
//! the manifest spells them, and the excluded directories from its
//! `[plugin.workspace] exclude_dirs`.

use std::collections::BTreeMap;
use std::path::Path;

use crate::daemon::manifest::{extension_of, under_excluded_dir, DiscoveredPlugins};
use crate::project_walk;

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
    /// Directory names the plugin's walk skips at any depth, copied from its
    /// `plugin.toml` `[plugin.workspace] exclude_dirs`. Like `extensions`, it
    /// says which files the absent plugin would claim; it is not a capability.
    pub exclude_dirs: &'static [&'static str],
}

impl CatalogueEntry {
    /// The exact command that installs this language's plugin:
    /// `g-mesh plugins install <language>`.
    ///
    /// `g-mesh init`/`reindex` print it on a `PluginAbsent` language's stderr
    /// line (`cli::language_outcome_lines`); the subcommand ships in the same
    /// release. It is derived from the
    /// language id rather than stored, so an entry cannot name one language
    /// and install another.
    pub fn install_command(&self) -> String {
        format!("g-mesh plugins install {}", self.language)
    }
}

/// What a bulk walk did for one language (ADR 0021). The install command is
/// not stored: [`CatalogueEntry::install_command`] derives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanguageOutcome {
    /// A discovered plugin walked it to the end. `files` is its `File`-node
    /// count; a discovered language with no files is `Indexed { files: 0 }`.
    Indexed { files: usize },
    /// A catalogue language with no discovered plugin and at least one file
    /// in the project. Not an error. `files` is `None` when it was not counted.
    PluginAbsent { files: Option<usize> },
    /// A discovered plugin that could not be used; nothing of the language is
    /// in the index. `error` is the full error chain.
    Failed { error: String },
}

/// Every catalogued language, in a fixed order. Adding a language is adding
/// one entry here.
pub const CATALOGUE: &[CatalogueEntry] = &[
    CatalogueEntry {
        language: "typescript",
        extensions: &[".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"],
        exclude_dirs: &["node_modules", "dist"],
    },
    CatalogueEntry {
        language: "python",
        extensions: &[".py", ".pyi"],
        exclude_dirs: &[
            ".venv",
            "venv",
            "__pycache__",
            ".tox",
            ".mypy_cache",
            "site-packages",
            "node_modules",
        ],
    },
    CatalogueEntry { language: "rust", extensions: &[".rs"], exclude_dirs: &["target"] },
    CatalogueEntry { language: "go", extensions: &[".go"], exclude_dirs: &["vendor", "testdata"] },
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

/// How many files under `root` each absent catalogue language would index
/// (ADR 0021): only languages in [`missing`], and only those with at least one
/// file. A file counts for the language [`absent_for_path`] names, unless it is
/// under one of that language's own `exclude_dirs` - never another language's,
/// which would hide `dist/app.py` from an absent Python. Walks with the same
/// walker as `g-mesh status`, pruning the baseline plus the directories every
/// absent language excludes. Empty, without walking, when no plugin is absent.
pub fn count_absent_files(root: &Path, discovered: &DiscoveredPlugins) -> BTreeMap<&'static str, usize> {
    count_absent_files_in(CATALOGUE, root, discovered)
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

fn count_absent_files_in<'a>(
    table: &'a [CatalogueEntry],
    root: &Path,
    discovered: &DiscoveredPlugins,
) -> BTreeMap<&'a str, usize> {
    let mut counts = BTreeMap::new();
    let absent = missing_in(table, discovered);
    let Some((first, rest)) = absent.split_first() else {
        return counts;
    };
    // `under_excluded_dir` takes owned names, as the manifests hold them.
    let exclude_dirs = |entry: &CatalogueEntry| -> Vec<String> {
        entry.exclude_dirs.iter().map(|dir| (*dir).to_string()).collect()
    };
    let mut pruned = exclude_dirs(first);
    for entry in rest {
        pruned.retain(|dir| entry.exclude_dirs.contains(&dir.as_str()));
    }
    let excluded: BTreeMap<&str, Vec<String>> =
        absent.iter().map(|entry| (entry.language, exclude_dirs(entry))).collect();

    for walked in project_walk::project_files(root, &pruned) {
        let Some(entry) = absent_for_path_in(table, discovered, &walked.relative) else {
            continue;
        };
        if excluded.get(entry.language).is_some_and(|dirs| under_excluded_dir(&walked.relative, dirs)) {
            continue;
        }
        *counts.entry(entry.language).or_insert(0) += 1;
    }
    counts
}

#[cfg(test)]
mod tests;
