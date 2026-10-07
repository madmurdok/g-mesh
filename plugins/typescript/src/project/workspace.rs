//! The project's own workspace packages: which directory a bare package name
//! like `@scope/math` is, and which paths its entry could be.
//!
//! Packages outside the workspace (`react`, `node:fs`, anything only under
//! `node_modules`) are not here: none of their files is indexed, so an edge
//! to one would point at nothing.
//!
//! Workspace globs (`pnpm-workspace.yaml`'s `packages:`, package.json
//! `workspaces` as an array or yarn's `{ packages: [...] }`) are matched
//! against [`DirectoryTree`], the directories that hold an indexed file
//! (design note section 2.2). So `node_modules` is never a member, a
//! symlinked package has the spelling the SDK walk listed
//! (`docs/adr/0025-project-walk-follows-symlinks.md`), and a package
//! directory with no indexable file is not a package.

use std::collections::{BTreeMap, BTreeSet};

use crate::project::exports::exports_targets;
use crate::project::jsonc::Json;
use crate::project::paths;

/// How deep a `**` glob segment descends below where it starts.
const MAX_GLOB_DEPTH: usize = 6;

/// A workspace member.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspacePackage {
    /// Project-relative directory holding its package.json.
    pub dir: String,
    /// That package.json.
    pub manifest: Json,
}

/// Every directory that holds an indexed file, and their ancestors up to the
/// root `""`.
#[derive(Debug, Default, Clone)]
pub struct DirectoryTree {
    /// All of them, sorted.
    pub dirs: BTreeSet<String>,
    /// Each directory's child directory names, sorted by byte order.
    children: BTreeMap<String, Vec<String>>,
}

impl DirectoryTree {
    /// The tree around `files` (project-relative file paths).
    pub fn from_files<'a>(files: impl IntoIterator<Item = &'a str>) -> Self {
        let mut dirs = BTreeSet::from([String::new()]);
        for file in files {
            for dir in paths::ancestors(paths::dirname(file)) {
                if !dirs.insert(dir.to_string()) {
                    // Its ancestors are already in.
                    break;
                }
            }
        }
        // A sorted set lists one parent's children in name order: they share
        // the `parent/` prefix and differ only after it.
        let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for dir in dirs.iter().filter(|dir| !dir.is_empty()) {
            children
                .entry(paths::dirname(dir).to_string())
                .or_default()
                .push(paths::basename(dir).to_string());
        }
        Self { dirs, children }
    }

    /// Whether `dir` is in the tree.
    pub fn contains(&self, dir: &str) -> bool {
        self.dirs.contains(dir)
    }

    /// The child names a glob segment may match under `dir`: dot-directories
    /// and `node_modules` are never workspace members.
    fn glob_children(&self, dir: &str) -> impl Iterator<Item = &str> {
        self.children
            .get(dir)
            .into_iter()
            .flatten()
            .map(String::as_str)
            .filter(|name| !name.starts_with('.') && *name != "node_modules")
    }
}

/// `@scope/pkg/sub` split into the package name and the subpath it addresses
/// (`"."` for the package itself, otherwise `./sub`), or `None` when the
/// specifier is not a package name: relative, absolute, a URL such as
/// `node:fs`, a `#private` key, or one with an empty segment.
pub fn parse_bare_specifier(specifier: &str) -> Option<(String, String)> {
    if specifier.is_empty() || specifier.starts_with('.') || specifier.starts_with('/') {
        return None;
    }
    if specifier.starts_with('#') || has_url_scheme(specifier) {
        return None;
    }
    let segments: Vec<&str> = specifier.split('/').collect();
    if segments.iter().any(|segment| segment.is_empty()) {
        return None;
    }
    let name_length = if specifier.starts_with('@') { 2 } else { 1 };
    if segments.len() < name_length {
        return None;
    }
    let name = segments[..name_length].join("/");
    let rest = &segments[name_length..];
    let subpath = if rest.is_empty() { ".".to_string() } else { format!("./{}", rest.join("/")) };
    Some((name, subpath))
}

/// `scheme:` at the start, as a URL spells it (`node:`, `data:`, `https:`).
fn has_url_scheme(specifier: &str) -> bool {
    let mut chars = specifier.chars();
    if !chars.next().is_some_and(|first| first.is_ascii_alphabetic()) {
        return false;
    }
    for char in chars {
        if char == ':' {
            return true;
        }
        if !(char.is_ascii_alphanumeric() || matches!(char, '+' | '-' | '.')) {
            return false;
        }
    }
    false
}

/// Every project-relative path `subpath` of `package` could name, most
/// specific first: the `exports` map, then (for the root) `source`, `main`,
/// `module`, `types`, `typings`, then the source conventions `src/index` and
/// `index` (for a subpath: the subpath itself, then under `src/`).
///
/// The conventions matter most: a declared entry usually points into build
/// output (`dist/`) that an unbuilt checkout lacks and a built one ignores,
/// so it is never in the existence set and cannot shadow the source.
pub fn package_entry_targets(package: &WorkspacePackage, subpath: &str) -> Vec<String> {
    let mut targets = exports_targets(package.manifest.get("exports"), subpath);

    if subpath == "." {
        for field in ["source", "main", "module", "types", "typings"] {
            if let Some(entry) = package.manifest.get(field).and_then(Json::as_str) {
                targets.push(entry.to_string());
            }
        }
        targets.push("src/index".to_string());
        targets.push("index".to_string());
    } else {
        let relative = &subpath[2..];
        targets.push(relative.to_string());
        targets.push(format!("src/{relative}"));
    }

    let mut resolved: Vec<String> = Vec::new();
    for target in targets {
        if let Some(inside) = paths::inside(&package.dir, &target) {
            if !resolved.contains(&inside) {
                resolved.push(inside);
            }
        }
    }
    resolved
}

/// The workspace globs: `pnpm-workspace.yaml`'s first, then the root
/// package.json's `workspaces` (an array, or yarn's object with `packages`).
/// A repo carrying both gets the union.
pub fn workspace_patterns(pnpm_workspace: Option<&str>, root_manifest: Option<&Json>) -> Vec<String> {
    let mut patterns = pnpm_workspace.map(pnpm_workspace_patterns).unwrap_or_default();
    let list = root_manifest.and_then(|manifest| manifest.get("workspaces")).and_then(|workspaces| {
        workspaces.as_array().or_else(|| workspaces.get("packages").and_then(Json::as_array))
    });
    for entry in list.into_iter().flatten() {
        if let Some(pattern) = entry.as_str().map(str::trim).filter(|pattern| !pattern.is_empty()) {
            patterns.push(pattern.to_string());
        }
    }
    patterns
}

/// The `packages:` list of a pnpm-workspace.yaml, read line by line: a flat
/// list of strings under one top-level key, as a block sequence or a flow
/// sequence (`packages: [a, "b"]`). Comments and blank lines are skipped.
pub fn pnpm_workspace_patterns(text: &str) -> Vec<String> {
    let mut patterns = Vec::new();
    let mut in_packages = false;
    for raw_line in text.split('\n') {
        let raw_line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let line = raw_line.replace('\t', "  ");
        let trimmed = line.trim();
        if trimmed.is_empty() || line.trim_start().starts_with('#') {
            continue;
        }

        if !line.starts_with(char::is_whitespace) {
            let value = packages_key_value(&line);
            in_packages = value.is_some();
            if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
                patterns.extend(flow_sequence_items(value));
                in_packages = false;
            }
            continue;
        }

        if !in_packages {
            continue;
        }
        if let Some(item) = line.trim_start().strip_prefix('-') {
            let item = item.trim();
            if !item.is_empty() {
                let value = scalar_value(item);
                if !value.is_empty() {
                    patterns.push(value);
                }
            }
        }
    }
    patterns
}

/// What follows `packages:` on a top-level line, or `None` for another key.
fn packages_key_value(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("packages")?;
    let rest = rest.trim_start().strip_prefix(':')?;
    Some(rest.trim_start())
}

/// The items of a one-line flow sequence `[a, 'b', "c"]`.
pub fn flow_sequence_items(text: &str) -> Vec<String> {
    let text = text.trim();
    let Some(inner) = text.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) else {
        return Vec::new();
    };
    inner.split(',').map(|item| scalar_value(item.trim())).filter(|item| !item.is_empty()).collect()
}

/// A YAML scalar: the inside of a quoted one, or a plain one cut at a ` #`
/// comment and trimmed.
pub fn scalar_value(raw: &str) -> String {
    for quote in ['"', '\''] {
        if raw.len() >= 2 && raw.starts_with(quote) && raw.ends_with(quote) {
            return raw[1..raw.len() - 1].to_string();
        }
    }
    let uncommented = raw.find(" #").map_or(raw, |at| &raw[..at]);
    uncommented.trim().to_string()
}

/// The workspace packages by declared name. Patterns starting with `!`
/// exclude the directories they match. A directory whose package.json is
/// missing, malformed or has no string `name` is skipped; on a duplicate
/// name the first directory found wins (patterns in order, each pattern's
/// matches sorted).
pub fn workspace_packages(
    patterns: &[String],
    tree: &DirectoryTree,
    manifests: &BTreeMap<String, Json>,
) -> BTreeMap<String, WorkspacePackage> {
    let excluded: Vec<Vec<GlobToken>> =
        patterns.iter().filter_map(|pattern| pattern.strip_prefix('!')).map(compile_glob).collect();

    let mut packages = BTreeMap::new();
    for pattern in patterns.iter().filter(|pattern| !pattern.starts_with('!')) {
        for dir in expand_pattern(pattern, tree) {
            if excluded.iter().any(|glob| glob_matches(glob, &dir)) {
                continue;
            }
            let Some(manifest) = manifests.get(&dir) else {
                continue;
            };
            let Some(name) = manifest.get("name").and_then(Json::as_str) else {
                continue;
            };
            packages
                .entry(name.to_string())
                .or_insert_with(|| WorkspacePackage { dir: dir.clone(), manifest: manifest.clone() });
        }
    }
    packages
}

/// The directories of `tree` that `pattern` names, in match order without
/// repeats, the root excluded. `*` and `?` match within one segment, `**`
/// any number of segments (at most [`MAX_GLOB_DEPTH`] below its start). A
/// pattern with a `..` segment names nothing.
pub fn expand_pattern(pattern: &str, tree: &DirectoryTree) -> Vec<String> {
    let segments: Vec<&str> =
        pattern.split('/').filter(|segment| !segment.is_empty() && *segment != ".").collect();
    if segments.is_empty() || segments.contains(&"..") {
        return Vec::new();
    }

    let mut dirs = vec![String::new()];
    for segment in segments {
        let mut next: Vec<String> = Vec::new();
        for dir in &dirs {
            if segment == "**" {
                next.push(dir.clone());
                descendants(dir, tree, MAX_GLOB_DEPTH, &mut next);
            } else if segment.contains('*') || segment.contains('?') {
                let glob = compile_glob(segment);
                for child in tree.glob_children(dir) {
                    if glob_matches(&glob, child) {
                        next.push(paths::join(dir, child));
                    }
                }
            } else {
                let candidate = paths::join(dir, segment);
                if tree.contains(&candidate) {
                    next.push(candidate);
                }
            }
        }
        let mut seen = BTreeSet::new();
        next.retain(|dir| seen.insert(dir.clone()));
        dirs = next;
    }
    dirs.retain(|dir| !dir.is_empty());
    dirs
}

/// Every directory below `dir`, depth-first, at most `depth` levels down.
fn descendants(dir: &str, tree: &DirectoryTree, depth: usize, out: &mut Vec<String>) {
    if depth == 0 {
        return;
    }
    for child in tree.glob_children(dir) {
        let child_dir = paths::join(dir, child);
        out.push(child_dir.clone());
        descendants(&child_dir, tree, depth - 1, out);
    }
}

/// One element of a compiled workspace glob.
#[derive(Debug, Clone, PartialEq)]
enum GlobToken {
    /// One literal character.
    Literal(char),
    /// `?`: one character other than `/`.
    AnyChar,
    /// `*`: any run of characters other than `/`.
    Star,
    /// `**` not followed by `/`: anything.
    DoubleStar,
    /// `**/`: nothing, or anything ending in `/`.
    DoubleStarSlash,
}

/// `pattern` (a leading `./` dropped) as glob tokens.
fn compile_glob(pattern: &str) -> Vec<GlobToken> {
    let glob = pattern.strip_prefix("./").unwrap_or(pattern);
    let chars: Vec<char> = glob.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let char = chars[index];
        if char == '*' && chars.get(index + 1) == Some(&'*') {
            if chars.get(index + 2) == Some(&'/') {
                tokens.push(GlobToken::DoubleStarSlash);
                index += 3;
            } else {
                tokens.push(GlobToken::DoubleStar);
                index += 2;
            }
            continue;
        }
        tokens.push(match char {
            '*' => GlobToken::Star,
            '?' => GlobToken::AnyChar,
            _ => GlobToken::Literal(char),
        });
        index += 1;
    }
    tokens
}

/// Whether the whole of `text` matches `glob`.
fn glob_matches(glob: &[GlobToken], text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    matches_from(glob, &chars)
}

fn matches_from(glob: &[GlobToken], text: &[char]) -> bool {
    let Some((token, rest)) = glob.split_first() else {
        return text.is_empty();
    };
    match token {
        GlobToken::Literal(expected) => text.first() == Some(expected) && matches_from(rest, &text[1..]),
        GlobToken::AnyChar => text.first().is_some_and(|char| *char != '/') && matches_from(rest, &text[1..]),
        GlobToken::Star => {
            let limit = text.iter().position(|char| *char == '/').unwrap_or(text.len());
            (0..=limit).any(|taken| matches_from(rest, &text[taken..]))
        }
        GlobToken::DoubleStar => (0..=text.len()).any(|taken| matches_from(rest, &text[taken..])),
        GlobToken::DoubleStarSlash => {
            matches_from(rest, text)
                || (1..=text.len()).any(|taken| text[taken - 1] == '/' && matches_from(rest, &text[taken..]))
        }
    }
}
