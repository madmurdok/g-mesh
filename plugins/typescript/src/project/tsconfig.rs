//! tsconfig/jsconfig `paths` aliases (`@/utils` -> `src/utils.ts`), with
//! `baseUrl` and the `extends` chain applied.
//!
//! A file's aliases come from the nearest directory at or above it holding a
//! `tsconfig.json` or `jsconfig.json` (in that order); that config wins
//! outright even when it declares no `paths`, which is how tsc scopes a
//! config to its directory. [`super::TsProject::load`] reads every such
//! config once and records its effective rules per directory.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::project::exports::match_wildcard;
use crate::project::jsonc::{parse_jsonc, Json};
use crate::project::paths;

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
/// the project are dropped.
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
                Some(capture) => target.replace('*', capture),
                None => target.clone(),
            };
            if let Some(inside) = paths::inside(&config.resolve_dir, &substituted) {
                if !candidates.contains(&inside) {
                    candidates.push(inside);
                }
            }
        }
    }
    candidates
}

/// The config files an `extends` entry written in `config` could name,
/// project-relative, in the order tsc tries them. An entry may name the
/// file, drop its `.json`, or name a directory holding a `tsconfig.json`.
///
/// A relative entry (`./`, `../`) resolves against the config's directory.
/// A package-named one (`@tsconfig/node18/tsconfig.json`) resolves under
/// `node_modules` of the config's directory and each ancestor up to the
/// root. Candidates outside the project, and absolute entries, are dropped.
pub fn extends_candidates(config: &str, entry: &str) -> Vec<String> {
    let config_dir = paths::dirname(config);
    let bases: Vec<String> = if entry.starts_with("./") || entry.starts_with("../") {
        vec![paths::join(config_dir, entry)]
    } else if entry.is_empty() || entry.starts_with('/') || entry.starts_with('.') {
        Vec::new()
    } else {
        paths::ancestors(config_dir)
            .map(|dir| paths::join(&paths::join(dir, "node_modules"), entry))
            .collect()
    };

    let mut candidates = Vec::new();
    for base in bases {
        let base = paths::normalize(&base);
        if paths::escapes(&base) {
            continue;
        }
        if base.ends_with(".json") {
            candidates.push(base);
        } else {
            candidates.push(format!("{base}.json"));
            candidates.push(format!("{base}/tsconfig.json"));
        }
    }
    candidates
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
    let base_url = base_url.replace('\\', "/");
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
pub struct TsconfigReader<'a> {
    root: &'a Path,
    raw: HashMap<String, Option<RawTsconfig>>,
    effective: HashMap<String, Option<Arc<EffectiveConfig>>>,
    notes: &'a mut Vec<String>,
}

impl<'a> TsconfigReader<'a> {
    /// A reader for configs under `root`, appending what it skips to `notes`.
    pub fn new(root: &'a Path, notes: &'a mut Vec<String>) -> Self {
        Self { root, raw: HashMap::new(), effective: HashMap::new(), notes }
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
            let target = extends_candidates(config, entry)
                .into_iter()
                .find(|candidate| config_is_file(self.root, candidate));
            let Some(target) = target else {
                self.notes.push(format!("{config}: `extends` target {entry:?} not found, skipped"));
                continue;
            };
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
                self.notes.push(format!("{config}: unreadable ({err}), skipped"));
                None
            }
        };
        self.raw.insert(config.to_string(), raw.clone());
        raw
    }
}

/// Whether the project-relative `path` is a regular file (symlinks followed).
fn config_is_file(root: &Path, path: &str) -> bool {
    root.join(path).is_file()
}
