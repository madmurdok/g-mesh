//! The resolution facts of a [`ProjectContext`] ([`RustFacts`]) and what
//! changed between two of them ([`resolution_delta`]): which files a
//! `Cargo.toml` save re-keys, and which importers it can make resolve
//! differently.
//!
//! Design: `docs/architecture/gm-509-selective-config-reindex.md`, section 3.6
//! and its GM-544 paragraph.
//!
//! - **What extraction reads from the model** is a file's own container key
//!   ([`ProjectContext::container_for`]), whether a key exists
//!   ([`ProjectContext::has_container`]) and which bare first segments name a
//!   crate of this project ([`ProjectContext::crates`]). Dependencies,
//!   versions, features and every other manifest table reach none of them, so
//!   an edit of those answers `Unchanged`.
//! - **A changed set of crate names answers `Unknown`.** A path such as
//!   `some_crate::f()` needs no `use`, so the files whose calls resolve
//!   differently once `some_crate` is (or stops being) one of ours carry no
//!   `IMPORTS` edge a selector could name.
//! - **In-crate imports are matched by their stored target key**, never by
//!   their text: `crate::`, `self::` and `super::` mean whatever the
//!   importer's own key says, and an importer whose key moved is selected as
//!   a file anyway.

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

/// A Rust container key's separator, `crate::a::b`.
const SEPARATOR: &str = "::";

/// What a [`ProjectContext`]'s extraction reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RustFacts {
    format: u32,
    /// Every crate key, which a bare first path segment is checked against.
    crates: BTreeSet<String>,
    /// Each module-tree file's container key, and every key that exists.
    containers: ContainerFacts,
}

impl RustFacts {
    /// The facts of `project`.
    pub fn of(project: &ProjectContext) -> Self {
        Self {
            format: FORMAT,
            crates: project.crates.iter().map(|krate| krate.key.clone()).collect(),
            containers: ContainerFacts {
                files: project.files.iter().map(|(path, (key, _))| (path.to_string(), key.clone())).collect(),
                keys: project.container_keys.clone(),
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

/// What a `Cargo.toml` save changed for resolution, `previous` being the
/// facts the index was built from and `project` the reloaded model:
///
/// - the same crates and module tree: `Unchanged`;
/// - another set of crate names: `Unknown` (see this module's doc);
/// - otherwise [`container_delta`]'s answer: every file whose key moved,
///   importers of every added or removed key, and specifiers under every
///   added key, plus [`with_parent_targets`].
///
/// `Unknown` also when `previous` is unreadable.
pub fn resolution_delta(previous: &str, project: &ProjectContext) -> ResolutionDelta {
    let Some(old) = RustFacts::decode(previous) else {
        return ResolutionDelta::Unknown {
            reason: "the previous resolution facts are unreadable".to_string(),
        };
    };
    delta(&old, &RustFacts::of(project))
}

/// [`resolution_delta`] between two decoded facts.
pub fn delta(old: &RustFacts, new: &RustFacts) -> ResolutionDelta {
    if old.crates != new.crates {
        let added: Vec<&String> = new.crates.difference(&old.crates).collect();
        let removed: Vec<&String> = old.crates.difference(&new.crates).collect();
        return ResolutionDelta::Unknown {
            reason: format!(
                "the crate names changed (added {added:?}, removed {removed:?}): a path such as `name::f()` \
                 needs no `use`, so no import selector names every file that resolves it differently"
            ),
        };
    }
    with_parent_targets(
        container_delta(&old.containers, &new.containers, SEPARATOR),
        &old.containers,
        &new.containers,
    )
}

/// Adds, for every added key `p::c`, the importers whose stored target is
/// its parent `p`: `use p::c;` draws its `IMPORTS` edge onto `p`, and a
/// second one onto `p::c` only while `p::c` is a key
/// ([`ProjectContext::has_container`]), so the importer that gains the second
/// edge is found by the first one's target. A removed key's importers
/// already carry an edge onto it, which [`container_delta`] selects.
fn with_parent_targets(
    delta: ResolutionDelta,
    old: &ContainerFacts,
    new: &ContainerFacts,
) -> ResolutionDelta {
    let ResolutionDelta::Affected { files, mut imports } = delta else { return delta };
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
