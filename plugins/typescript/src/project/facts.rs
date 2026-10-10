//! The resolution facts of a [`TsProject`] ([`TsFacts`]) and what changed
//! between two of them ([`resolution_delta`]): which importers a config edit
//! can make resolve differently.
//!
//! Design: `docs/architecture/gm-509-selective-config-reindex.md`, section 3.6.
//!
//! - **The existence set is not a fact.** It changes through presence events,
//!   not config edits, and the reloaded model's walk is adopted as is.
//! - **A selector names every specifier the edit can move**, never fewer:
//!   a package by its name, a tsconfig by its `paths` patterns (every
//!   non-relative specifier when a pattern is `*` or the targets' base moved),
//!   an `imports` map by `#`. Each is scoped to the importers whose nearest
//!   config is the edited one.

use std::collections::{BTreeMap, BTreeSet};

use g_mesh_plugin_sdk::wire::{ImportMatch, ImportSelector, Matcher, PathScope, ResolutionDelta};
use serde::{Deserialize, Serialize};

use crate::project::jsonc::Json;
use crate::project::paths;
use crate::project::tsconfig::EffectiveConfig;
use crate::project::workspace::WorkspacePackage;
use crate::project::TsProject;

/// The facts' format. A blob of another version is unreadable, which answers
/// `Unknown`.
const FORMAT: u32 = 1;

/// The package.json fields [`crate::project::workspace::package_entry_targets`]
/// reads besides `exports`.
const ENTRY_FIELDS: [&str; 5] = ["source", "main", "module", "types", "typings"];

/// What a [`TsProject`]'s resolution reads, existence aside.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TsFacts {
    format: u32,
    /// Workspace packages by name.
    packages: BTreeMap<String, PackageFacts>,
    /// Each config directory's effective `paths` (`None`: it declares none).
    tsconfig_by_dir: BTreeMap<String, Option<ConfigFacts>>,
    /// Each package.json directory's `imports` map, rendered (`None`: none).
    imports_by_dir: BTreeMap<String, Option<String>>,
}

/// One workspace package as its entry resolution reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PackageFacts {
    dir: String,
    /// `exports` and the [`ENTRY_FIELDS`] present, each rendered as JSON.
    fields: BTreeMap<String, String>,
}

/// One effective config's alias rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ConfigFacts {
    resolve_dir: String,
    paths: Vec<(String, Vec<String>)>,
}

impl TsFacts {
    /// The facts of `project`.
    pub fn of(project: &TsProject) -> Self {
        Self {
            format: FORMAT,
            packages: project
                .packages
                .iter()
                .map(|(name, package)| (name.clone(), package_facts(package)))
                .collect(),
            tsconfig_by_dir: project
                .tsconfig_by_dir
                .iter()
                .map(|(dir, config)| (dir.clone(), config.as_deref().map(config_facts)))
                .collect(),
            imports_by_dir: project
                .imports_by_dir
                .iter()
                .map(|(dir, imports)| {
                    (
                        dir.clone(),
                        imports.as_ref().map(|imports| render(&Json::Object(imports.imports.clone()))),
                    )
                })
                .collect(),
        }
    }

    /// The blob core stores.
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("resolution facts always serialize")
    }

    /// The facts `blob` holds, or `None` for a blob of another format.
    pub fn decode(blob: &str) -> Option<Self> {
        serde_json::from_str::<Self>(blob).ok().filter(|facts| facts.format == FORMAT)
    }
}

fn package_facts(package: &WorkspacePackage) -> PackageFacts {
    let fields = std::iter::once("exports")
        .chain(ENTRY_FIELDS)
        .filter_map(|field| package.manifest.get(field).map(|value| (field.to_string(), render(value))))
        .collect();
    PackageFacts { dir: package.dir.clone(), fields }
}

fn config_facts(config: &EffectiveConfig) -> ConfigFacts {
    ConfigFacts {
        resolve_dir: config.resolve_dir.clone(),
        paths: config.paths.iter().map(|entry| (entry.pattern.clone(), entry.targets.clone())).collect(),
    }
}

/// `value` as compact JSON, object keys in source order (which resolution
/// reads). A number renders as `0`: nothing resolution reads is a number.
fn render(value: &Json) -> String {
    let mut out = String::new();
    render_into(value, &mut out);
    out
}

fn render_into(value: &Json, out: &mut String) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        Json::Number => out.push('0'),
        Json::String(text) => out.push_str(&serde_json::to_string(text).expect("a string always serializes")),
        Json::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                render_into(item, out);
            }
            out.push(']');
        }
        Json::Object(entries) => {
            out.push('{');
            for (index, (key, item)) in entries.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).expect("a string always serializes"));
                out.push(':');
                render_into(item, out);
            }
            out.push('}');
        }
    }
}

/// What changed for resolution between the facts `previous` (a blob) and
/// `project`.
///
/// - A package added, removed or with a changed projection: importers
///   anywhere whose specifier is the package name or under it.
/// - A config at `D` added, removed or changed: importers under `D` and under
///   no deeper config directory, old or new, whose specifier matches an old
///   or new pattern of `D` (and, when `D` gained or lost its config, of the
///   config that governed `D` from above). Every non-relative specifier when
///   a pattern starts with `*` or the targets' base directory moved.
/// - An `imports` map at `D` added, removed or changed: importers under `D`
///   and under no deeper package.json directory, old or new, whose specifier
///   starts with `#`.
///
/// `Unknown` when `previous` is unreadable or a config could not be read
/// from disk (an I/O failure may be transient; the reindex reads again).
pub fn resolution_delta(previous: &str, project: &TsProject) -> ResolutionDelta {
    if let Some(note) = project.notes.iter().find(|note| note.contains(crate::project::UNREADABLE_NOTE)) {
        return ResolutionDelta::Unknown { reason: format!("a resolution config is unreadable: {note}") };
    }
    let Some(old) = TsFacts::decode(previous) else {
        return ResolutionDelta::Unknown {
            reason: "the previous resolution facts are unreadable".to_string(),
        };
    };
    delta(&old, &TsFacts::of(project))
}

/// [`resolution_delta`] between two decoded facts.
pub fn delta(old: &TsFacts, new: &TsFacts) -> ResolutionDelta {
    let mut imports: Vec<ImportSelector> = Vec::new();
    let mut push = |selector: ImportSelector| {
        if !imports.contains(&selector) {
            imports.push(selector);
        }
    };

    for name in union(&old.packages, &new.packages) {
        if old.packages.get(name) != new.packages.get(name) {
            push(ImportSelector {
                importers: PathScope { under: String::new(), not_under: Vec::new() },
                by: ImportMatch::Specifier(Matcher::Under {
                    prefix: name.clone(),
                    separator: "/".to_string(),
                }),
            });
        }
    }

    let config_dirs = union(&old.tsconfig_by_dir, &new.tsconfig_by_dir);
    for dir in &config_dirs {
        let (before, after) = (old.tsconfig_by_dir.get(*dir), new.tsconfig_by_dir.get(*dir));
        if before == after {
            continue;
        }
        let mut governing: Vec<&ConfigFacts> =
            [before, after].into_iter().flatten().filter_map(Option::as_ref).collect();
        if before.is_none() || after.is_none() {
            governing.extend(inherited(&old.tsconfig_by_dir, dir));
            governing.extend(inherited(&new.tsconfig_by_dir, dir));
        }
        let base_moved = matches!((before, after), (Some(Some(before)), Some(Some(after))) if before.resolve_dir != after.resolve_dir);
        let importers = shadowed_scope(dir, &config_dirs);
        for matcher in pattern_matchers(&governing, base_moved) {
            push(ImportSelector { importers: importers.clone(), by: ImportMatch::Specifier(matcher) });
        }
    }

    let package_dirs = union(&old.imports_by_dir, &new.imports_by_dir);
    for dir in &package_dirs {
        if old.imports_by_dir.get(*dir) != new.imports_by_dir.get(*dir) {
            push(ImportSelector {
                importers: shadowed_scope(dir, &package_dirs),
                by: ImportMatch::Specifier(Matcher::StartsWith("#".to_string())),
            });
        }
    }

    if imports.is_empty() {
        ResolutionDelta::Unchanged
    } else {
        ResolutionDelta::Affected { files: Vec::new(), imports }
    }
}

fn union<'a, V>(old: &'a BTreeMap<String, V>, new: &'a BTreeMap<String, V>) -> BTreeSet<&'a String> {
    old.keys().chain(new.keys()).collect()
}

/// The config governing `dir` from a directory strictly above it, if any.
fn inherited<'a>(configs: &'a BTreeMap<String, Option<ConfigFacts>>, dir: &str) -> Option<&'a ConfigFacts> {
    paths::ancestors(dir).skip(1).find_map(|ancestor| configs.get(ancestor))?.as_ref()
}

/// The files under `dir` and under none of the deeper directories of `dirs`.
fn shadowed_scope(dir: &str, dirs: &BTreeSet<&String>) -> PathScope {
    let not_under =
        dirs.iter().filter(|deeper| is_strictly_under(deeper, dir)).map(|deeper| (*deeper).clone()).collect();
    PathScope { under: dir.to_string(), not_under }
}

/// Whether the directory `deeper` lies strictly below the directory `dir`.
fn is_strictly_under(deeper: &str, dir: &str) -> bool {
    deeper != dir && (dir.is_empty() || deeper.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/')))
}

/// One matcher per distinct pattern of `configs`: `StartsWith` the text
/// before a `*`, `Exact` for a pattern without one, and a lone `NonRelative`
/// when a pattern starts with `*` or `base_moved`.
fn pattern_matchers(configs: &[&ConfigFacts], base_moved: bool) -> Vec<Matcher> {
    let patterns: BTreeSet<&str> =
        configs.iter().flat_map(|config| config.paths.iter().map(|(pattern, _)| pattern.as_str())).collect();
    if patterns.is_empty() {
        return Vec::new();
    }
    if base_moved || patterns.iter().any(|pattern| pattern.starts_with('*')) {
        return vec![Matcher::NonRelative];
    }
    patterns
        .into_iter()
        .map(|pattern| match pattern.split_once('*') {
            Some((prefix, _)) => Matcher::StartsWith(prefix.to_string()),
            None => Matcher::Exact(pattern.to_string()),
        })
        .collect()
}
