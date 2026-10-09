//! The resolution facts of a [`ProjectContext`] ([`PyFacts`]) and what
//! changed between two of them ([`resolution_delta`]): which files a
//! `pyproject.toml` save re-keys, and which importers it can make resolve
//! differently.
//!
//! Design: `docs/architecture/gm-509-selective-config-reindex.md`, section 3.6
//! and its GM-544 paragraph.
//!
//! - **The roots decide everything a config edit can move.** A file's
//!   container key is a pure function of its path and the roots, and
//!   [`ProjectContext::has_container`] answers from those keys. With the same
//!   roots, every file keeps its key whatever else the save changed (a
//!   version, a dependency), so the answer is `Unchanged`. `setup.cfg` and
//!   `setup.py` are never read, so a save of either always is.
//! - **The file set is not a fact to compare.** It changes through presence
//!   events, which the model applies between loads (ADR 0023). When the
//!   roots did move, a file created since the facts were stored shows up as
//!   added and is selected: an over-selection, never a miss.
//! - **A relative import is matched by its stored target**, never by its
//!   text: its dots mean whatever the importer's own key says, and an
//!   importer whose key moved is selected as a file anyway.

use std::collections::BTreeSet;

use g_mesh_plugin_sdk::wire::{
    ImportMatch, ImportSelector, Matcher, PathScope, ResolutionDelta, TargetScopeKind,
};
use g_mesh_plugin_sdk::{container_delta, ContainerFacts};
use serde::{Deserialize, Serialize};

use super::ProjectContext;

/// The facts' format. A blob of another version is unreadable, which answers
/// `Unknown`.
const FORMAT: u32 = 1;

/// A Python container key's separator, `pkg.sub.mod`.
const SEPARATOR: &str = ".";

/// What a [`ProjectContext`]'s extraction reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PyFacts {
    format: u32,
    /// The resolved roots' directories, deepest first.
    roots: Vec<String>,
    /// Each file's container key, and every key that exists.
    containers: ContainerFacts,
}

impl PyFacts {
    /// The facts of `project`.
    pub fn of(project: &ProjectContext) -> Self {
        Self {
            format: FORMAT,
            roots: project.roots.iter().map(|root| root.dir.to_string()).collect(),
            containers: ContainerFacts {
                files: project.files.iter().map(|(path, info)| (path.to_string(), info_key(info))).collect(),
                keys: project.containers.keys().cloned().collect(),
            },
        }
    }

    /// The opaque blob core stores.
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("resolution facts always serialize")
    }

    /// The facts `blob` holds, or `None` for a blob of another format.
    pub fn decode(blob: &str) -> Option<Self> {
        serde_json::from_str::<Self>(blob).ok().filter(|facts| facts.format == FORMAT)
    }
}

/// A file's container key, which also fixes its parent and name (both are
/// cut from the key) and, with the path's own file name, its kind.
fn info_key(info: &super::ContainerInfo) -> String {
    use super::ContainerInfo;
    match info {
        ContainerInfo::Module { key, .. }
        | ContainerInfo::Package { key, .. }
        | ContainerInfo::Stub { key }
        | ContainerInfo::Orphan { key } => key.clone(),
    }
}

/// What a `pyproject.toml`, `setup.cfg` or `setup.py` save changed for
/// resolution, `previous` being the facts the index was built from and
/// `project` the reloaded model:
///
/// - the same roots: `Unchanged` (see this module's doc);
/// - otherwise [`container_delta`]'s answer: every file whose key moved,
///   importers of every added or removed key, and specifiers under every
///   added key, plus [`with_import_selectors`].
///
/// `Unknown` when `previous` is unreadable.
pub fn resolution_delta(previous: &str, project: &ProjectContext) -> ResolutionDelta {
    let Some(old) = PyFacts::decode(previous) else {
        return ResolutionDelta::Unknown {
            reason: "the previous resolution facts are unreadable".to_string(),
        };
    };
    delta(&old, &PyFacts::of(project))
}

/// [`resolution_delta`] between two decoded facts.
pub fn delta(old: &PyFacts, new: &PyFacts) -> ResolutionDelta {
    if old.roots == new.roots {
        return ResolutionDelta::Unchanged;
    }
    with_import_selectors(
        container_delta(&old.containers, &new.containers, SEPARATOR),
        &old.containers,
        &new.containers,
    )
}

/// Adds two selectors [`container_delta`] does not draw, both because an
/// absolute import's output depends on [`ProjectContext::has_container`]:
///
/// - for every added key `p.c`, the importers whose stored target is its
///   parent `p`: `from p import c` draws its `IMPORTS` edge onto `p`, and a
///   second one onto `p.c` only while `p.c` is a key, so the importer that
///   gains the second edge - a relative one included - is found by the
///   first one's target;
/// - for every removed key `k`, the importers whose specifier is under `k`:
///   `import k.x` binds `k` as one of ours only while `k` is a key, and when
///   `k.x` never was one its edge is an `external_module` no target selector
///   reaches.
fn with_import_selectors(
    delta: ResolutionDelta,
    old: &ContainerFacts,
    new: &ContainerFacts,
) -> ResolutionDelta {
    let ResolutionDelta::Affected { files, mut imports } = delta else { return delta };
    for key in old.keys.difference(&new.keys) {
        imports.push(ImportSelector {
            importers: PathScope { under: String::new(), not_under: Vec::new() },
            by: ImportMatch::Specifier(Matcher::Under {
                prefix: key.clone(),
                separator: SEPARATOR.to_string(),
            }),
        });
    }
    let parents: BTreeSet<&str> = new
        .keys
        .difference(&old.keys)
        .filter_map(|key| key.rsplit_once(SEPARATOR).map(|(parent, _)| parent))
        .collect();
    for parent in parents {
        imports.push(ImportSelector {
            importers: PathScope { under: String::new(), not_under: Vec::new() },
            by: ImportMatch::Target {
                scope_kind: TargetScopeKind::Container,
                matcher: Matcher::Exact(parent.to_string()),
            },
        });
    }
    ResolutionDelta::Affected { files, imports }
}
