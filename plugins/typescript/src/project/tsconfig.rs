//! tsconfig/jsconfig `paths` aliases (`@/utils` -> `src/utils.ts`), with
//! `baseUrl` and the `extends` chain applied.
//!
//! A file's aliases come from the nearest directory at or above it holding a
//! `tsconfig.json` or `jsconfig.json` (in that order); that config wins
//! outright even when it declares no `paths`, which is how tsc scopes a
//! config to its directory. [`super::TsProject::load`] reads every such
//! config once and records its effective rules per directory.

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::extractor::grammar::EXTENSIONS as SOURCE_EXTENSIONS;
use crate::project::exports::match_wildcard;
use crate::project::jsonc::{parse_json, parse_jsonc, Json};
use crate::project::{paths, EXCLUDE_DIRS};

/// Config file names a directory is searched for, in priority order.
pub const CONFIG_FILE_NAMES: [&str; 2] = ["tsconfig.json", "jsconfig.json"];

/// One `paths` key and its targets, in declaration order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathsEntry {
    /// The alias, e.g. `@/*`.
    pub pattern: String,
    /// What it expands to, e.g. `["./src/*"]`.
    pub targets: Vec<String>,
}

/// A config's alias rules after its `extends` chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveConfig {
    /// Project-relative directory the targets resolve against (`""` is the
    /// root): the declaring config's `baseUrl`, or its own directory.
    pub resolve_dir: String,
    /// The entries, in declaration order.
    pub paths: Vec<PathsEntry>,
}

/// The fields read off one config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTsconfig {
    /// `extends`, a string or (TS 5.0+) an array, always as a list.
    pub extends: Vec<String>,
    /// `compilerOptions.baseUrl`.
    pub base_url: Option<String>,
    /// `compilerOptions.paths`; `None` when the config declares none.
    pub paths: Option<Vec<PathsEntry>>,
}

/// `text` as a config, or `None` when it is not JSONC describing an object.
pub fn read_raw_tsconfig(text: &str) -> Option<RawTsconfig> {
    let parsed = parse_jsonc(text)?;
    parsed.as_object()?;
    let compiler_options = parsed.get("compilerOptions").filter(|options| options.as_object().is_some());
    Some(RawTsconfig {
        extends: parsed.get("extends").map(extends_list).unwrap_or_default(),
        base_url: compiler_options
            .and_then(|options| options.get("baseUrl"))
            .and_then(Json::as_str)
            .map(str::to_string),
        paths: compiler_options.and_then(|options| options.get("paths")).and_then(paths_entries),
    })
}

/// `extends` as a list: a string is one entry, non-string array items are
/// dropped, anything else is none.
pub fn extends_list(value: &Json) -> Vec<String> {
    match value {
        Json::String(entry) => vec![entry.clone()],
        Json::Array(items) => items.iter().filter_map(Json::as_str).map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

/// `paths` as entries, or `None` when it is not an object. A key whose value
/// is not an array, or holds no string, is dropped.
pub fn paths_entries(value: &Json) -> Option<Vec<PathsEntry>> {
    let entries = value.as_object()?;
    Some(
        entries
            .iter()
            .filter_map(|(pattern, targets)| {
                let targets: Vec<String> =
                    targets.as_array()?.iter().filter_map(Json::as_str).map(str::to_string).collect();
                (!targets.is_empty()).then(|| PathsEntry { pattern: pattern.clone(), targets })
            })
            .collect(),
    )
}

/// Every project-relative path `specifier` could name under `config`, in
/// declaration order, without repeats. Every matching key contributes; the
/// existence set picks. A key without `*` is an exact alias. Targets leaving
/// the project are dropped. A target naming the project root is kept (its
/// `index` files are the candidates). tsc substitutes no empty capture: a
/// target with a `*` names nothing for it (`@/` under `"@/*": ["./src/*"]`),
/// a target without one is used as written (`@r/` under `["./"]` names the
/// root).
pub fn expand_paths_candidates(config: &EffectiveConfig, specifier: &str) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    for entry in &config.paths {
        let capture = if entry.pattern.contains('*') {
            match match_wildcard(&entry.pattern, specifier) {
                Some(capture) => Some(capture),
                None => continue,
            }
        } else if entry.pattern == specifier {
            None
        } else {
            continue;
        };
        for target in &entry.targets {
            let substituted = match capture {
                Some("") if target.contains('*') => continue,
                Some(capture) => target.replace('*', capture),
                None => target.clone(),
            };
            if let Some(inside) = paths::inside_or_root(&config.resolve_dir, &substituted) {
                if !candidates.contains(&inside) {
                    candidates.push(inside);
                }
            }
        }
    }
    candidates
}

/// The specifiers whose expansion under `config` ([`expand_paths_candidates`])
/// may include `address`, in declaration order without repeats: a wildcard
/// entry's pattern with the capture its target pattern takes from `address`
/// (`@/*` -> `src/*`, `src/x` -> `@/x`), an exact entry's own pattern when
/// one of its targets is `address`. A wildcard entry whose target has no `*`
/// names that one path for every capture, so it gives no specifier.
pub fn specifiers_targeting(config: &EffectiveConfig, address: &str) -> Vec<String> {
    let mut specifiers: Vec<String> = Vec::new();
    for entry in &config.paths {
        for target in &entry.targets {
            let Some(pattern) = paths::inside_or_root(&config.resolve_dir, target) else {
                continue;
            };
            let specifier = if entry.pattern.contains('*') {
                match match_wildcard(&pattern, address) {
                    Some(capture) => entry.pattern.replacen('*', capture, 1),
                    None => continue,
                }
            } else if pattern == address {
                entry.pattern.clone()
            } else {
                continue;
            };
            if !specifiers.contains(&specifier) {
                specifiers.push(specifier);
            }
        }
    }
    specifiers
}

/// The config files a relative `extends` entry written in `config` could
/// name, project-relative, in the order tsc tries them.
///
/// A relative entry (`./`, `../`) resolves against the config's directory
/// and names a file: the path as written, then with `.json` appended unless
/// it already ends in `.json`. It never names a directory, so `../base`
/// does not reach `../base/tsconfig.json`, and `../` or `./..` names only
/// the parent's sibling `<parent>.json`.
/// An entry of exactly `.` or `..` is a module name for the config's own or
/// parent directory and names that directory's `tsconfig.json`; it may be
/// the project root's. Candidates outside the project are dropped, and so
/// is every other entry (a package name, an absolute path, an empty one):
/// [`TsconfigReader`] resolves those itself. As in tsc, `\\` is a separator
/// on every host.
pub fn extends_candidates(config: &str, entry: &str) -> Vec<String> {
    let config_dir = paths::dirname(config);
    let entry = paths::with_forward_slashes(entry);
    let entry = entry.as_str();
    if entry == "." || entry == ".." {
        return paths::inside_or_root(config_dir, entry)
            .map(|dir| {
                let dir = if dir == paths::ROOT { "" } else { dir.as_str() };
                vec![paths::join(dir, "tsconfig.json")]
            })
            .unwrap_or_default();
    }
    if !is_relative_extends(entry) {
        return Vec::new();
    }
    let base = paths::normalize(&paths::join(config_dir, entry));
    if paths::escapes(&base) {
        return Vec::new();
    }
    file_candidates(&base)
}

/// Whether a slash-normalized `extends` entry is `./x` or `../x`.
fn is_relative_extends(entry: &str) -> bool {
    entry.starts_with("./") || entry.starts_with("../")
}

/// The files an `extends` path names, in tsc's order: `base` as written,
/// then `base.json` unless it already ends in `.json`.
fn file_candidates(base: &str) -> Vec<String> {
    if base.ends_with(".json") {
        vec![base.to_string()]
    } else {
        vec![base.to_string(), format!("{base}.json")]
    }
}

/// The directory a config's own `paths` resolve against: `baseUrl` when it
/// declares one, else the config's own directory (tsc's rule for `paths`
/// without `baseUrl`). `None` when that lies outside the project.
///
/// As in tsc, on every host `\\` is a separator and a `baseUrl` starting
/// with `/` or a drive letter is absolute; a drive-less one takes `root`'s
/// drive.
pub fn own_resolve_dir(config: &str, base_url: Option<&str>, root: &Path) -> Option<String> {
    let config_dir = paths::dirname(config);
    let Some(base_url) = base_url else {
        return Some(config_dir.to_string());
    };
    let base_url = paths::with_forward_slashes(base_url);
    let joined = if paths::is_rooted(&base_url) {
        let relative = with_roots_drive(Path::new(&base_url), root).strip_prefix(root).ok()?.to_path_buf();
        paths::normalize(&relative.to_string_lossy().replace('\\', "/"))
    } else {
        paths::normalize(&paths::join(config_dir, &base_url))
    };
    match joined.as_str() {
        "." => Some(String::new()),
        _ if paths::escapes(&joined) => None,
        _ => Some(joined),
    }
}

/// `path` on `root`'s drive when `path` is rooted but names no drive (only
/// possible on Windows); otherwise `path` itself.
fn with_roots_drive(path: &Path, root: &Path) -> PathBuf {
    let has_prefix = |p: &Path| matches!(p.components().next(), Some(Component::Prefix(_)));
    match root.components().next() {
        Some(Component::Prefix(prefix)) if !has_prefix(path) => Path::new(prefix.as_os_str()).join(path),
        _ => path.to_path_buf(),
    }
}

/// Reads configs under one root, each file at most once, and resolves their
/// `extends` chains. Only [`super::TsProject::load`] uses it.
///
/// It also collects the watch paths of those chains
/// ([`TsconfigReader::into_watch_paths`]): every file an `extends`
/// resolution looked at, found or not, so that creating, editing or deleting
/// one reloads the model. Paths under [`EXCLUDE_DIRS`] (a package under
/// `node_modules`) are left out, so a change there is not seen until the
/// next reload.
pub struct TsconfigReader<'a> {
    root: &'a Path,
    /// `root` with symlinks resolved; `None` when it cannot be.
    canonical_root: Option<PathBuf>,
    raw: HashMap<String, Option<RawTsconfig>>,
    effective: HashMap<String, Option<Arc<EffectiveConfig>>>,
    /// package.json files read for `extends`, parsed (`None`: missing or not
    /// a JSON object).
    manifests: HashMap<String, Option<Json>>,
    watch_paths: BTreeSet<String>,
    notes: &'a mut Vec<String>,
}

/// Why an `extends` entry names no config.
enum ExtendsMiss {
    NotFound,
    OutsideProject,
}

/// The conditions tsc matches in a package's `exports` when it resolves an
/// `extends` entry; any other key is skipped.
const EXTENDS_CONDITIONS: [&str; 4] = ["node", "require", "types", "default"];

impl<'a> TsconfigReader<'a> {
    /// A reader for configs under `root`, appending what it skips to `notes`.
    pub fn new(root: &'a Path, notes: &'a mut Vec<String>) -> Self {
        Self {
            root,
            canonical_root: root.canonicalize().ok(),
            raw: HashMap::new(),
            effective: HashMap::new(),
            manifests: HashMap::new(),
            watch_paths: BTreeSet::new(),
            notes,
        }
    }

    /// The files the `extends` chains resolved so far depend on,
    /// project-relative: each path a resolution probed (found or not), and
    /// the in-project real path of a found target reached through a symlink.
    /// Never a path under [`EXCLUDE_DIRS`] or a file with a source extension.
    pub fn into_watch_paths(self) -> BTreeSet<String> {
        self.watch_paths
    }

    /// The config `dir` holds itself (`tsconfig.json` before
    /// `jsconfig.json`), project-relative, or `None`.
    pub fn own_config(&self, dir: &str) -> Option<String> {
        CONFIG_FILE_NAMES
            .iter()
            .map(|name| paths::join(dir, name))
            .find(|config| config_is_file(self.root, config))
    }

    /// `config`'s alias rules after its `extends` chain, or `None` when
    /// neither it nor anything it extends declares usable `paths`.
    ///
    /// tsc's merge: `paths` is inherited or replaced whole, never per key;
    /// later `extends` entries layer over earlier ones; only a config that
    /// declares `paths` picks their directory, so a `baseUrl` alone does not
    /// move inherited aliases. A `baseUrl` outside the project voids the
    /// config's own `paths` but keeps an ancestor's. A cyclic chain stops at
    /// the repeat.
    pub fn effective(&mut self, config: &str) -> Option<Arc<EffectiveConfig>> {
        let mut in_progress = Vec::new();
        self.resolve_effective(config, &mut in_progress)
    }

    fn resolve_effective(
        &mut self,
        config: &str,
        in_progress: &mut Vec<String>,
    ) -> Option<Arc<EffectiveConfig>> {
        if let Some(cached) = self.effective.get(config) {
            return cached.clone();
        }
        if in_progress.iter().any(|open| open == config) {
            self.notes.push(format!("{config}: cyclic `extends` chain, the repeat is skipped"));
            return None;
        }
        let Some(raw) = self.read_raw(config) else {
            self.effective.insert(config.to_string(), None);
            return None;
        };

        in_progress.push(config.to_string());
        let mut inherited = None;
        for entry in &raw.extends {
            let target = match self.resolve_extends(config, entry) {
                Ok(target) => target,
                Err(ExtendsMiss::NotFound) => {
                    self.notes.push(format!("{config}: `extends` target {entry:?} not found, skipped"));
                    continue;
                }
                Err(ExtendsMiss::OutsideProject) => {
                    self.notes.push(format!(
                        "{config}: `extends` target {entry:?} is outside the project, skipped"
                    ));
                    continue;
                }
            };
            self.record_real_path(&target);
            if let Some(contributed) = self.resolve_effective(&target, in_progress) {
                inherited = Some(contributed);
            }
        }
        in_progress.pop();

        let mut result = inherited;
        if let Some(own_paths) = raw.paths {
            match own_resolve_dir(config, raw.base_url.as_deref(), self.root) {
                Some(resolve_dir) => {
                    result = Some(Arc::new(EffectiveConfig { resolve_dir, paths: own_paths }));
                }
                None => self
                    .notes
                    .push(format!("{config}: `baseUrl` leaves the project, its own `paths` are ignored")),
            }
        }
        self.effective.insert(config.to_string(), result.clone());
        result
    }

    /// One config file's fields, read once. A missing file is `None` without
    /// a note; an unreadable or malformed one is noted.
    fn read_raw(&mut self, config: &str) -> Option<RawTsconfig> {
        if let Some(cached) = self.raw.get(config) {
            return cached.clone();
        }
        let raw = match std::fs::read_to_string(self.root.join(config)) {
            Ok(text) => {
                let raw = read_raw_tsconfig(&text);
                if raw.is_none() {
                    self.notes.push(format!("{config}: not a JSONC object, skipped"));
                }
                raw
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                self.notes.push(format!("{config}{}{err}), skipped", super::UNREADABLE_NOTE));
                None
            }
        };
        self.raw.insert(config.to_string(), raw.clone());
        raw
    }
}

impl TsconfigReader<'_> {
    /// The config file `entry`, written in `config`, names, as tsc 5.9
    /// resolves it:
    /// - `.`, `..`, `./x`, `../x`: [`extends_candidates`];
    /// - an absolute path (a POSIX root or a drive letter; a drive-less root
    ///   takes `root`'s drive): `p`, then `p.json`, when inside the project;
    /// - anything else is a package name (`.hidden/x` included): the
    ///   config's own package by name ([`Self::self_name_target`]), then
    ///   `node_modules` of the config's directory and each ancestor up to the
    ///   root, nearest first ([`Self::node_modules_target`]).
    ///
    /// Every path it probes is recorded as a watch path.
    fn resolve_extends(&mut self, config: &str, entry: &str) -> Result<String, ExtendsMiss> {
        let entry = paths::with_forward_slashes(entry);
        if entry == "." || entry == ".." || is_relative_extends(&entry) {
            return self.first_file(extends_candidates(config, &entry)).ok_or(ExtendsMiss::NotFound);
        }
        if entry.is_empty() {
            return Err(ExtendsMiss::NotFound);
        }
        if paths::is_rooted(&entry) {
            let base = self.rooted_inside(&entry).ok_or(ExtendsMiss::OutsideProject)?;
            return self.first_file(file_candidates(&base)).ok_or(ExtendsMiss::NotFound);
        }
        self.resolve_package_extends(config, &entry).ok_or(ExtendsMiss::NotFound)
    }

    /// The absolute `entry` as a project-relative path, or `None` when it
    /// lies outside the project. `entry` may be spelled against `root` as
    /// given or against its real path.
    fn rooted_inside(&self, entry: &str) -> Option<String> {
        let absolute = with_roots_drive(Path::new(entry), self.root);
        let relative = absolute
            .strip_prefix(self.root)
            .ok()
            .or_else(|| absolute.strip_prefix(self.canonical_root.as_ref()?).ok())?;
        let joined = paths::normalize(&relative.to_string_lossy().replace('\\', "/"));
        (!paths::escapes(&joined)).then_some(joined)
    }

    /// A package-named `entry` (`@scope/name/rest`, `name/rest`, `name`).
    fn resolve_package_extends(&mut self, config: &str, entry: &str) -> Option<String> {
        let (name, rest) = split_package_entry(entry)?;
        let ancestors: Vec<String> = paths::ancestors(paths::dirname(config)).map(str::to_string).collect();
        if let Some(found) = self.self_name_target(&ancestors, name, rest) {
            return Some(found);
        }
        ancestors.iter().find_map(|dir| self.node_modules_target(dir, name, rest))
    }

    /// `rest` of the config's own package when the nearest package.json at
    /// or above the config (`ancestors`, nearest first) is named `name` and
    /// declares `exports`. Only that nearest package.json counts.
    fn self_name_target(&mut self, ancestors: &[String], name: &str, rest: &str) -> Option<String> {
        for dir in ancestors {
            let manifest_path = paths::join(dir, "package.json");
            if !self.probe(&manifest_path) {
                continue;
            }
            let manifest = self.manifest(&manifest_path)?;
            if manifest.get("name").and_then(Json::as_str) != Some(name) {
                return None;
            }
            let exports = manifest.get("exports").filter(|exports| **exports != Json::Null)?;
            return self.config_exports_target(dir, exports, &exports_subpath(rest));
        }
        None
    }

    /// `name`/`rest` under `dir/node_modules`. A package.json with `exports`
    /// decides alone. Otherwise the bare name is `<name>.json`, then the
    /// package directory ([`Self::package_dir_target`]); a subpath is
    /// `rest.json` (or `rest` when it ends in `.json`), then the directory
    /// `rest`.
    fn node_modules_target(&mut self, dir: &str, name: &str, rest: &str) -> Option<String> {
        let modules = paths::join(dir, "node_modules");
        let package = paths::normalize(&paths::join(&modules, name));
        if paths::escapes(&package) {
            return None;
        }
        let manifest = self.manifest(&paths::join(&package, "package.json"));
        if let Some(exports) = manifest.as_ref().and_then(|m| m.get("exports")).filter(|e| **e != Json::Null)
        {
            return self.config_exports_target(&package, exports, &exports_subpath(rest));
        }
        if rest.is_empty() {
            let file = format!("{package}.json");
            return if self.probe(&file) { Some(file) } else { self.package_dir_target(&package) };
        }
        let sub = paths::normalize(&paths::join(&package, rest));
        if paths::escapes(&sub) {
            return None;
        }
        let file = if sub.ends_with(".json") { sub.clone() } else { format!("{sub}.json") };
        if self.probe(&file) {
            return Some(file);
        }
        self.package_dir_target(&sub)
    }

    /// The config a package directory names: its package.json `tsconfig`
    /// field `f` (the file `f`, then `f.json`, then `f/tsconfig.json`), else,
    /// also when that names nothing, the directory's `tsconfig.json`.
    fn package_dir_target(&mut self, dir: &str) -> Option<String> {
        let manifest = self.manifest(&paths::join(dir, "package.json"));
        let field = manifest.as_ref().and_then(|m| m.get("tsconfig")).and_then(Json::as_str);
        if let Some(base) = field.and_then(|field| paths::inside(dir, field)) {
            let mut candidates = file_candidates(&base);
            candidates.push(paths::join(&base, "tsconfig.json"));
            if let Some(found) = self.first_file(candidates) {
                return Some(found);
            }
        }
        let fallback = paths::join(dir, "tsconfig.json");
        self.probe(&fallback).then_some(fallback)
    }

    /// The file a package's `exports` maps `subpath` (`.` or `./rest`) to:
    /// the exact key, else the wildcard key with the longest prefix before
    /// its `*`. A subpath it does not export names nothing; there is no
    /// fallback inside that package.
    fn config_exports_target(&mut self, package: &str, exports: &Json, subpath: &str) -> Option<String> {
        let entries = match exports {
            Json::Object(entries) if entries.iter().any(|(key, _)| key == "." || key.starts_with("./")) => {
                entries
            }
            _ if subpath == "." => return self.exports_value_target(package, exports, None),
            _ => return None,
        };
        if let Some(exact) = exports.get(subpath) {
            return self.exports_value_target(package, exact, None);
        }
        let mut best: Option<(usize, &Json, &str)> = None;
        for (key, value) in entries {
            let Some(capture) = match_wildcard(key, subpath) else { continue };
            let prefix = key.find('*').unwrap_or(0);
            if best.is_none_or(|(longest, _, _)| prefix > longest) {
                best = Some((prefix, value, capture));
            }
        }
        let (_, value, capture) = best?;
        self.exports_value_target(package, value, Some(capture))
    }

    /// One `exports` value: a `./` string is the exact file (with `*` taken
    /// by `capture`) when it exists; an array is its first element that
    /// resolves; an object is its first key among [`EXTENDS_CONDITIONS`], in
    /// declaration order.
    fn exports_value_target(&mut self, package: &str, value: &Json, capture: Option<&str>) -> Option<String> {
        match value {
            Json::String(target) => {
                let target = paths::with_forward_slashes(target);
                if !target.starts_with("./") {
                    return None;
                }
                let target = match capture {
                    Some(capture) => target.replace('*', capture),
                    None => target,
                };
                let path = paths::inside(package, &target)?;
                self.probe(&path).then_some(path)
            }
            Json::Array(items) => {
                items.iter().find_map(|item| self.exports_value_target(package, item, capture))
            }
            Json::Object(entries) => {
                let (_, chosen) =
                    entries.iter().find(|(key, _)| EXTENDS_CONDITIONS.contains(&key.as_str()))?;
                self.exports_value_target(package, chosen, capture)
            }
            _ => None,
        }
    }

    /// The first of `candidates` that is a file, probing each in order.
    fn first_file(&mut self, candidates: Vec<String>) -> Option<String> {
        candidates.into_iter().find(|candidate| self.probe(candidate))
    }

    /// Whether `path` is a file; records it as a watch path either way.
    fn probe(&mut self, path: &str) -> bool {
        self.record(path);
        config_is_file(self.root, path)
    }

    /// The package.json at `path`, read once and recorded as a watch path.
    fn manifest(&mut self, path: &str) -> Option<Json> {
        if let Some(cached) = self.manifests.get(path) {
            return cached.clone();
        }
        self.record(path);
        let manifest = std::fs::read_to_string(self.root.join(path))
            .ok()
            .and_then(|text| parse_json(&text))
            .filter(|manifest| manifest.as_object().is_some());
        self.manifests.insert(path.to_string(), manifest.clone());
        manifest
    }

    /// Records the in-project real path of `target` when a symlink makes it
    /// differ, so a save through the real path is seen too.
    fn record_real_path(&mut self, target: &str) {
        let Some(canonical_root) = &self.canonical_root else { return };
        let Ok(real) = self.root.join(target).canonicalize() else { return };
        let Ok(relative) = real.strip_prefix(canonical_root) else { return };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if relative != target {
            self.record(&relative);
        }
    }

    /// Adds `path` to the watch paths unless the core contract forbids it:
    /// outside the project, under [`EXCLUDE_DIRS`], or a source file.
    fn record(&mut self, path: &str) {
        let normalized = paths::normalize(path);
        if paths::escapes(&normalized) || normalized != path {
            return;
        }
        let excluded = path.split('/').rev().skip(1).any(|segment| EXCLUDE_DIRS.contains(&segment));
        let source = paths::basename(path).rsplit_once('.').is_some_and(|(_, ext)| {
            SOURCE_EXTENSIONS.iter().any(|known| known.strip_prefix('.') == Some(ext))
        });
        if !excluded && !source {
            self.watch_paths.insert(path.to_string());
        }
    }
}

/// `name`/`rest` of a package-named `extends` entry (`@scope/name` takes two
/// segments), or `None` with an empty segment.
fn split_package_entry(entry: &str) -> Option<(&str, &str)> {
    let name_segments = if entry.starts_with('@') { 2 } else { 1 };
    let mut split = entry.len();
    let mut seen = 0;
    for (index, char) in entry.char_indices() {
        if char == '/' {
            seen += 1;
            if seen == name_segments {
                split = index;
                break;
            }
        }
    }
    let (name, rest) = (&entry[..split], entry[split..].strip_prefix('/').unwrap_or(""));
    let segments_ok = |text: &str| text.split('/').all(|segment| !segment.is_empty());
    (segments_ok(name) && name.split('/').count() == name_segments && (rest.is_empty() || segments_ok(rest)))
        .then_some((name, rest))
}

/// The `exports` key a package subpath is looked up under.
fn exports_subpath(rest: &str) -> String {
    if rest.is_empty() {
        ".".to_string()
    } else {
        format!("./{rest}")
    }
}

/// Whether the project-relative `path` is a regular file (symlinks followed).
fn config_is_file(root: &Path, path: &str) -> bool {
    root.join(path).is_file()
}
