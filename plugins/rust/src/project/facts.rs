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

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch tree, removed on drop. Unique per call: `cargo test` runs
    /// these concurrently in one process.
    struct Tree(std::path::PathBuf);

    impl Tree {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("g-mesh-plugin-rust-facts-{}-{unique}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn write(&self, path: &str, contents: &str) -> &Self {
            let full = self.0.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, contents).unwrap();
            self
        }

        fn load(&self) -> ProjectContext {
            ProjectContext::load(&self.0).unwrap()
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const ALPHA: &str = "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n";

    /// A two-member workspace: `alpha` (`lib.rs` declaring `a`, plus a
    /// `lib2.rs` no `[lib]` names yet and an `extra.rs` only `lib2.rs`
    /// declares) and `beta`.
    fn workspace() -> Tree {
        let tree = Tree::new();
        tree.write("Cargo.toml", "[workspace]\nmembers = [\"alpha\", \"beta\"]\n")
            .write("alpha/Cargo.toml", ALPHA)
            .write("alpha/src/lib.rs", "pub mod a;\n")
            .write("alpha/src/a.rs", "pub fn f() {}\n")
            .write("alpha/src/lib2.rs", "pub mod a;\npub mod extra;\n")
            .write("alpha/src/extra.rs", "pub fn g() {}\n")
            .write("beta/Cargo.toml", "[package]\nname = \"beta\"\nversion = \"0.1.0\"\n")
            .write("beta/src/lib.rs", "use alpha::extra;\n");
        tree
    }

    fn file(path: &str) -> PathScope {
        PathScope { under: path.to_string(), not_under: Vec::new() }
    }

    fn target(key: &str) -> ImportSelector {
        ImportSelector {
            importers: file(""),
            by: ImportMatch::Target {
                scope_kind: TargetScopeKind::Container,
                matcher: Matcher::Exact(key.to_string()),
            },
        }
    }

    fn specifier_under(key: &str) -> ImportSelector {
        ImportSelector {
            importers: file(""),
            by: ImportMatch::Specifier(Matcher::Under {
                prefix: key.to_string(),
                separator: "::".to_string(),
            }),
        }
    }

    /// The delta of the save that turned the model `before` into `tree`'s
    /// current one, through the encoded blob core stores.
    fn saved(before: &ProjectContext, tree: &Tree) -> ResolutionDelta {
        resolution_delta(&RustFacts::of(before).encode(), &tree.load())
    }

    /// A version bump, a new dependency and a feature table read nothing
    /// extraction reads.
    ///
    /// Control: make [`delta`] return `container_delta` of empty old facts
    /// (or drop the `crates`/`containers` comparison): this answers
    /// `Affected`.
    #[test]
    fn a_dependency_and_version_edit_answers_unchanged() {
        let tree = workspace();
        let before = tree.load();
        tree.write(
            "alpha/Cargo.toml",
            "[package]\nname = \"alpha\"\nversion = \"0.2.0\"\n\n[dependencies]\nserde = \"1\"\n\n[features]\nx = []\n",
        );
        tree.write(
            "Cargo.toml",
            "[workspace]\nmembers = [\"alpha\", \"beta\"]\n\n[workspace.package]\nversion = \"9.0.0\"\n",
        );
        assert_eq!(saved(&before, &tree), ResolutionDelta::Unchanged);
    }

    /// Moving `alpha`'s crate root re-keys both roots and the file only the
    /// new root declares; `a.rs` keeps its key. The added key `alpha::extra`
    /// selects the importers of its parent `alpha`, where `use alpha::extra;`
    /// drew its edge while `extra` was not a module.
    ///
    /// Control: return `delta` unchanged from `with_parent_targets`: the
    /// `target("alpha")` selector is missing.
    #[test]
    fn a_lib_path_move_selects_the_re_keyed_files_and_the_importers_of_the_added_keys_parent() {
        let tree = workspace();
        let before = tree.load();
        tree.write("alpha/Cargo.toml", &format!("{ALPHA}\n[lib]\npath = \"src/lib2.rs\"\n"));
        assert_eq!(
            saved(&before, &tree),
            ResolutionDelta::Affected {
                files: vec![file("alpha/src/extra.rs"), file("alpha/src/lib.rs"), file("alpha/src/lib2.rs")],
                imports: vec![target("alpha::extra"), specifier_under("alpha::extra"), target("alpha")],
            }
        );
    }

    /// A `mod` item added since the facts were stored (GM-507: the model
    /// learns it at the next `Cargo.toml` load) is selected like any added
    /// key; its removal selects the importers of the removed key only.
    #[test]
    fn a_module_added_or_removed_since_the_facts_is_selected_on_the_next_save() {
        let tree = workspace();
        let before = tree.load();
        tree.write("alpha/src/lib.rs", "pub mod a;\npub mod extra;\n");
        let added = saved(&before, &tree);
        assert_eq!(
            added,
            ResolutionDelta::Affected {
                files: vec![file("alpha/src/extra.rs")],
                imports: vec![target("alpha::extra"), specifier_under("alpha::extra"), target("alpha")],
            }
        );

        let before = tree.load();
        tree.write("alpha/src/lib.rs", "pub mod a;\n");
        assert_eq!(
            saved(&before, &tree),
            ResolutionDelta::Affected {
                files: vec![file("alpha/src/extra.rs")],
                imports: vec![target("alpha::extra")]
            },
            "a removed key draws no parent selector: its importers carry an edge onto it"
        );
    }

    /// The owner's decision for GM-544: a changed set of crate names is a
    /// whole-language reindex, whether a crate is renamed or a member with a
    /// new name joins or leaves.
    ///
    /// Control: drop the `old.crates != new.crates` early return in
    /// [`delta`]: a rename answers `Affected`.
    #[test]
    fn a_changed_set_of_crate_names_answers_unknown() {
        let tree = workspace();
        let before = tree.load();
        tree.write("alpha/Cargo.toml", &ALPHA.replace("\"alpha\"", "\"alpha2\""));
        assert!(matches!(saved(&before, &tree), ResolutionDelta::Unknown { .. }), "a rename");

        let tree = workspace();
        let before = tree.load();
        tree.write("Cargo.toml", "[workspace]\nmembers = [\"alpha\", \"beta\", \"gamma\"]\n")
            .write("gamma/Cargo.toml", "[package]\nname = \"gamma\"\n")
            .write("gamma/src/lib.rs", "");
        assert!(matches!(saved(&before, &tree), ResolutionDelta::Unknown { .. }), "a member added");

        let tree = workspace();
        let before = tree.load();
        tree.write("Cargo.toml", "[workspace]\nmembers = [\"alpha\"]\n");
        assert!(matches!(saved(&before, &tree), ResolutionDelta::Unknown { .. }), "a member removed");
    }

    /// A blob that is not this format's facts answers `Unknown`, never a
    /// guess.
    #[test]
    fn unreadable_previous_facts_answer_unknown() {
        let tree = workspace();
        let project = tree.load();
        let other_format = RustFacts::of(&project).encode().replacen("\"format\":1", "\"format\":2", 1);
        assert_ne!(other_format, RustFacts::of(&project).encode(), "the blob spells its format as expected");
        for blob in ["", "not json", "{}", other_format.as_str()] {
            assert!(
                matches!(resolution_delta(blob, &project), ResolutionDelta::Unknown { .. }),
                "{blob:?} was read as facts"
            );
        }
        assert_eq!(RustFacts::decode(&RustFacts::of(&project).encode()), Some(RustFacts::of(&project)));
    }
}
