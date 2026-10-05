//! The project model [`TsProject`] the extractor resolves import specifiers
//! against, and the resolver itself ([`TsProject::resolve`]).
//!
//! Design: `docs/architecture/gm-324-typescript-rust-port.md`, section 2.
//! Presence: `docs/adr/0023-project-model-tracks-file-presence.md` and
//! `docs/adr/0026-batch-created-files-notification.md`. Walk and symlinks:
//! `docs/adr/0025-project-walk-follows-symlinks.md`.
//!
//! - **Reads happen only in [`TsProject::load`].** `extract` and the presence
//!   hook touch no disk, so resolution is a pure function of the model.
//! - **The existence set** is the SDK walk's file list, kept current by
//!   [`TsProject::file_presence_changed`]. A resolution candidate counts only
//!   when it is in the set.
//! - **Configs are read once.** Workspace packages, tsconfig `paths` per
//!   directory and package.json `imports` per directory change only through
//!   a reload: their files are [`WATCH_FILES`], and core reindexes the
//!   language when one is saved (ADR 0008).
//! - **Lookups walk up** the importer's directories against per-directory
//!   maps, so a directory created after the load needs no model update.
//! - **A bad config is a note, never an error.** An unreadable or malformed
//!   file is skipped and recorded in [`TsProject::notes`].

pub mod exports;
pub mod jsonc;
pub mod paths;
pub mod resolve;
pub mod tsconfig;
pub mod workspace;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use g_mesh_plugin_sdk::{walk_project, RelPath};

use crate::extractor::grammar::EXTENSIONS;
use crate::project::exports::PackageImports;
use crate::project::jsonc::{parse_json, Json};
use crate::project::tsconfig::{EffectiveConfig, TsconfigReader};
use crate::project::workspace::{workspace_packages, workspace_patterns, DirectoryTree, WorkspacePackage};

/// Directories the walk never descends into, on top of the SDK's own
/// baseline. Must equal `plugin.toml`'s `[plugin.workspace] exclude_dirs`.
pub const EXCLUDE_DIRS: [&str; 2] = ["node_modules", "dist"];

/// The files whose content the model is built from, as `plugin.toml`'s
/// `[plugin.workspace] watch_files` globs: a save of one reloads the model.
pub const WATCH_FILES: [&str; 4] =
    ["package.json", "tsconfig*.json", "jsconfig*.json", "pnpm-workspace.yaml"];

/// What the extractor knows about the project around a file.
#[derive(Debug, Default, Clone)]
pub struct TsProject {
    /// Every source file this plugin indexes, project-relative.
    pub existence: BTreeSet<RelPath>,
    /// Workspace packages by declared name.
    pub packages: BTreeMap<String, WorkspacePackage>,
    /// Directories holding a tsconfig/jsconfig, to that config's effective
    /// `paths` (`None`: the config declares none, and still shadows any
    /// config further up).
    pub tsconfig_by_dir: BTreeMap<String, Option<Arc<EffectiveConfig>>>,
    /// Directories holding a package.json, to its `imports` map (`None`: it
    /// has none or is malformed, and still shadows any package further up).
    pub imports_by_dir: BTreeMap<String, Option<PackageImports>>,
    /// What the load skipped and why, one line each.
    pub notes: Vec<String>,
}

impl TsProject {
    /// Builds the model for `root`: the SDK walk for the existence set, then
    /// every package.json, tsconfig.json/jsconfig.json (with their `extends`
    /// chains) and the root pnpm-workspace.yaml in the directories that hold
    /// an indexed file.
    ///
    /// Never fails on a config file: a bad one is a note.
    pub fn load(root: &Path) -> anyhow::Result<Self> {
        let extensions: Vec<String> = EXTENSIONS.iter().map(|ext| (*ext).to_string()).collect();
        let exclude: Vec<String> = EXCLUDE_DIRS.iter().map(|dir| (*dir).to_string()).collect();
        let existence: BTreeSet<RelPath> = walk_project(root, &extensions, &exclude).into_iter().collect();
        let tree = DirectoryTree::from_files(existence.iter().map(RelPath::as_str));

        let mut notes = Vec::new();
        let mut manifests: BTreeMap<String, Json> = BTreeMap::new();
        let mut imports_by_dir = BTreeMap::new();
        let mut tsconfig_by_dir = BTreeMap::new();
        {
            let mut configs = TsconfigReader::new(root, &mut notes);
            for dir in &tree.dirs {
                if let Some(config) = configs.own_config(dir) {
                    tsconfig_by_dir.insert(dir.clone(), configs.effective(&config));
                }
            }
        }

        for dir in &tree.dirs {
            let manifest_path = paths::join(dir, "package.json");
            let Some(text) = read_text(root, &manifest_path, &mut notes) else {
                continue;
            };
            let manifest = parse_json(&text).filter(|manifest| manifest.as_object().is_some());
            let imports = match &manifest {
                Some(manifest) => manifest
                    .get("imports")
                    .and_then(Json::as_object)
                    .map(|imports| PackageImports { dir: dir.clone(), imports: imports.to_vec() }),
                None => {
                    notes.push(format!("{manifest_path}: not a JSON object, skipped"));
                    None
                }
            };
            imports_by_dir.insert(dir.clone(), imports);
            if let Some(manifest) = manifest {
                manifests.insert(dir.clone(), manifest);
            }
        }

        let pnpm = read_text(root, "pnpm-workspace.yaml", &mut notes)
            .or_else(|| read_text(root, "pnpm-workspace.yml", &mut notes));
        let patterns = workspace_patterns(pnpm.as_deref(), manifests.get(""));
        let packages = workspace_packages(&patterns, &tree, &manifests);

        Ok(Self { existence, packages, tsconfig_by_dir, imports_by_dir, notes })
    }

    /// The file `specifier`, written in the file `from`, names, or `None`
    /// when it names nothing indexed. See [`resolve::resolve`] for the order.
    pub fn resolve(&self, specifier: &str, from: &RelPath) -> Option<RelPath> {
        resolve::resolve(self, specifier, from)
    }

    /// Records that `path` now exists or no longer does. Idempotent; nothing
    /// else in the model moves (configs change only through a reload).
    pub fn file_presence_changed(&mut self, path: &RelPath, present: bool) {
        if present {
            if !self.existence.contains(path) {
                self.existence.insert(path.clone());
            }
        } else {
            self.existence.remove(path);
        }
    }

    /// The effective `paths` of the config nearest the project-relative
    /// directory `dir`, or `None` when that config declares none or there is
    /// no config at or above `dir`.
    pub fn tsconfig_for(&self, dir: &str) -> Option<&EffectiveConfig> {
        paths::ancestors(dir).find_map(|ancestor| self.tsconfig_by_dir.get(ancestor))?.as_deref()
    }

    /// The `imports` map of the package.json nearest the project-relative
    /// directory `dir`, or `None` when that package.json has none or there
    /// is no package.json at or above `dir`.
    pub fn imports_for(&self, dir: &str) -> Option<&PackageImports> {
        paths::ancestors(dir).find_map(|ancestor| self.imports_by_dir.get(ancestor))?.as_ref()
    }
}

/// The text of the project-relative file `path`, or `None` when it does not
/// exist as a file (silently) or cannot be read as UTF-8 (noted).
fn read_text(root: &Path, path: &str, notes: &mut Vec<String>) -> Option<String> {
    let absolute = root.join(path);
    if !absolute.is_file() {
        return None;
    }
    match std::fs::read_to_string(absolute) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            notes.push(format!("{path}: unreadable ({err}), skipped"));
            None
        }
    }
}
