//! Reading `Cargo.toml` as data - never by invoking `cargo` - and resolving
//! `[workspace] members`/`exclude` globs against the filesystem.
//!
//! # Why not `cargo metadata`
//!
//! `cargo metadata` is the *correct* answer to "what crates does this
//! workspace have" - it is cargo's own resolution, macro expansion and all.
//! It is also a process spawn per `load_project`, on every `workspaceChanged`,
//! against a toolchain the plugin cannot assume is even installed (the
//! design doc's constraint: "structural tiers may not" depend on one). The
//! task this module implements is explicit about the trade: parse
//! `Cargo.toml` with the `toml` crate, and do not invoke cargo. What that
//! gives up is exactly the parts of cargo's own resolution this module does
//! not attempt - see this crate's `project` module doc, "What is not
//! modeled".
//!
//! # Permissive parsing
//!
//! [`RawCargoToml`] and its fields deserialize only what this plugin reads
//! and ignore everything else via serde's default "unknown fields are
//! skipped" behaviour (no `deny_unknown_fields` anywhere in this module) -
//! the same rule `plugins/sdk`'s own `manifest` module follows for
//! `plugin.toml`: a `Cargo.toml` gaining a table this plugin does not
//! understand (`[patch]`, `[features]`, a new `[workspace.foo]`) must never
//! stop the project model from loading.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The handful of `Cargo.toml` fields this plugin's project model needs.
/// Every other table (`[dependencies]`, `[features]`, `[profile.*]`, ...) is
/// parsed into nothing, by the absence of a field for it - see this module's
/// doc.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RawCargoToml {
    pub package: Option<RawPackage>,
    pub lib: Option<RawTarget>,
    #[serde(default, rename = "bin")]
    pub bins: Vec<RawTarget>,
    pub workspace: Option<RawWorkspace>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RawPackage {
    pub name: String,
}

/// `[lib]` or one `[[bin]]` entry. `name` and `path` are both optional on the
/// wire, per Cargo's own defaulting rules - resolved by the caller
/// (`crate::project::targets`), not here, because the default depends on
/// which target kind this is and on the package name, neither of which this
/// struct alone knows.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RawTarget {
    pub name: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RawWorkspace {
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// Reads and parses one `Cargo.toml`. `Err` on anything unreadable or
/// unparsable - the caller (`ProjectContext::load`) treats that as "no crate
/// here" and keeps going, per this module's doc on why a plugin never hard
/// fails on a manifest it cannot use.
pub(crate) fn read(path: &Path) -> anyhow::Result<RawCargoToml> {
    let contents = fs::read_to_string(path)?;
    Ok(toml::from_str(&contents)?)
}

/// Resolves `[workspace] members`/`exclude` glob patterns against `root`
/// into the directories they match.
///
/// Cargo's own glob syntax (the `glob` crate, full `**`, character classes)
/// is not implemented - only a single `*` wildcard per path segment,
/// matched against directory names one path component at a time
/// (`crates/*` walks `crates/`'s own entries; it does not recurse further).
/// That covers the overwhelmingly common shape of a workspace glob
/// (`members = ["crates/*"]`) without a new dependency - `plugins/sdk`
/// already brings `ignore` for `.gitignore` semantics, which is a different
/// problem (matching *files* against layered ignore rules, not enumerating
/// *directories* a glob names) and does not expose a general glob matcher
/// this module could reuse. A pattern with no `*` at all is matched as a
/// literal path and must be a directory to count.
pub(crate) fn resolve_globs(root: &Path, patterns: &[String]) -> BTreeSet<PathBuf> {
    let mut matches = BTreeSet::new();
    for pattern in patterns {
        matches.extend(resolve_one_glob(root, pattern));
    }
    matches
}

fn resolve_one_glob(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut current = vec![root.to_path_buf()];
    for segment in pattern.split('/').filter(|segment| !segment.is_empty()) {
        let mut next = Vec::new();
        for dir in &current {
            if segment.contains('*') {
                let Ok(entries) = fs::read_dir(dir) else { continue };
                let mut matched: Vec<PathBuf> = entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_type().is_ok_and(|file_type| file_type.is_dir()))
                    .filter(|entry| segment_matches(segment, &entry.file_name().to_string_lossy()))
                    .map(|entry| entry.path())
                    .collect();
                matched.sort();
                next.extend(matched);
            } else {
                let candidate = dir.join(segment);
                if candidate.is_dir() {
                    next.push(candidate);
                }
            }
        }
        current = next;
    }
    current
}

/// Whether `name` matches `pattern`, where `*` in `pattern` matches any
/// number (including zero) of characters and every other character must
/// match literally. Small and recursive rather than a dependency: the only
/// alphabet is "literal" and "star", and a workspace glob segment is at most
/// a few characters.
fn segment_matches(pattern: &str, name: &str) -> bool {
    fn go(pattern: &[u8], name: &[u8]) -> bool {
        match pattern.first() {
            None => name.is_empty(),
            Some(b'*') => (0..=name.len()).any(|i| go(&pattern[1..], &name[i..])),
            Some(&c) => name.first() == Some(&c) && go(&pattern[1..], &name[1..]),
        }
    }
    go(pattern.as_bytes(), name.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pattern_with_no_star_matches_literally() {
        assert!(segment_matches("crates", "crates"));
        assert!(!segment_matches("crates", "crate"));
    }

    #[test]
    fn a_star_matches_any_run_including_empty() {
        assert!(segment_matches("*", ""));
        assert!(segment_matches("*", "anything"));
        assert!(segment_matches("crate-*", "crate-a"));
        assert!(segment_matches("crate-*", "crate-"));
        assert!(!segment_matches("crate-*", "other"));
        assert!(segment_matches("*-crate", "a-crate"));
        assert!(segment_matches("a*b*c", "aXbYc"));
        assert!(!segment_matches("a*b*c", "aXbYd"));
    }
}
