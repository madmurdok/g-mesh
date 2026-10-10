//! Resolution facts for a project model keyed by container: which file sits
//! in which container, and which container keys exist. Shared by the plugins
//! whose imports name containers (a Rust module path, a Python dotted
//! module), so each states only how its model projects onto this shape.
//!
//! Design: `docs/architecture/gm-509-selective-config-reindex.md`, section 3.5.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::wire::{ImportMatch, ImportSelector, Matcher, PathScope, ResolutionDelta, TargetScopeKind};

/// The part of a container-keyed project model that resolution reads: every
/// indexed file's container key, and every key that exists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerFacts {
    /// Project-relative file path to the key of the container it belongs to.
    pub files: BTreeMap<String, String>,
    /// Every container key the model knows, including those no file maps to.
    pub keys: BTreeSet<String>,
}

/// What changed between two [`ContainerFacts`], as a [`ResolutionDelta`].
///
/// - `files`: every path whose key changed, was added or was removed, each as
///   an exact [`PathScope`];
/// - `imports`, across the whole project: `Target { container, Exact(k) }`
///   for every removed or added key, plus `Specifier(Under { k, separator })`
///   for every added key, which catches an import that named nothing before
///   (an `external_module`) and now resolves.
///
/// Equal facts answer [`ResolutionDelta::Unchanged`].
pub fn container_delta(old: &ContainerFacts, new: &ContainerFacts, separator: &str) -> ResolutionDelta {
    let mut files: Vec<PathScope> = Vec::new();
    let paths: BTreeSet<&String> = old.files.keys().chain(new.files.keys()).collect();
    for path in paths {
        if old.files.get(path) != new.files.get(path) {
            files.push(PathScope { under: path.clone(), not_under: Vec::new() });
        }
    }

    let whole_project = || PathScope { under: String::new(), not_under: Vec::new() };
    let mut imports: Vec<ImportSelector> = Vec::new();
    for key in old.keys.symmetric_difference(&new.keys) {
        imports.push(ImportSelector {
            importers: whole_project(),
            by: ImportMatch::Target {
                scope_kind: TargetScopeKind::Container,
                matcher: Matcher::Exact(key.clone()),
            },
        });
    }
    for key in new.keys.difference(&old.keys) {
        imports.push(ImportSelector {
            importers: whole_project(),
            by: ImportMatch::Specifier(Matcher::Under {
                prefix: key.clone(),
                separator: separator.to_string(),
            }),
        });
    }

    if files.is_empty() && imports.is_empty() {
        ResolutionDelta::Unchanged
    } else {
        ResolutionDelta::Affected { files, imports }
    }
}
