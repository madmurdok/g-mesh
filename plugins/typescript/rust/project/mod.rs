//! The project model [`TsProject`] the extractor reads.
//!
//! For now it records which files exist and nothing else: the extractor's
//! declaration pass needs no project knowledge. Module resolution
//! (tsconfig, workspaces, package exports) builds on this set.

use std::collections::BTreeSet;
use std::path::Path;

use g_mesh_plugin_sdk::{walk_project, RelPath};

use crate::extractor::grammar::EXTENSIONS;

/// Directories the walk never descends into, on top of the SDK's own
/// baseline. Must equal `plugin.toml`'s `[plugin.workspace] exclude_dirs`.
pub const EXCLUDE_DIRS: [&str; 2] = ["node_modules", "dist"];

/// What the extractor knows about the project around a file.
#[derive(Debug, Default, Clone)]
pub struct TsProject {
    /// Every source file this plugin claims, project-relative.
    pub existence: BTreeSet<RelPath>,
}

impl TsProject {
    /// Walks `root` once for the files this plugin claims. Never fails: an
    /// empty project is an empty set.
    pub fn load(root: &Path) -> anyhow::Result<Self> {
        let extensions: Vec<String> = EXTENSIONS.iter().map(|ext| (*ext).to_string()).collect();
        let exclude: Vec<String> = EXCLUDE_DIRS.iter().map(|dir| (*dir).to_string()).collect();
        let existence = walk_project(root, &extensions, &exclude).into_iter().collect();
        Ok(Self { existence })
    }
}
