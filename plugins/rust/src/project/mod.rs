//! `ProjectContext`: the Rust plugin's [`Extractor::Project`](g_mesh_plugin_sdk::Extractor::Project) -
//! every crate `Cargo.toml`/workspace membership names, and which container
//! key owns each `.rs` file, computed without invoking `cargo`.
//!
//! # Scope: this is GM-285, not GM-286
//!
//! This module answers "what is the module tree" - a question about the
//! project as a whole, asked once per [`Extractor::load_project`] and again
//! on every `workspaceChanged`. It does not answer "what does this file
//! declare" - that is per-file, asked on every `extract`, and it is
//! tree-sitter-rust's job (GM-286), not this one's. The seam between the two
//! is [`ProjectContext::container_for`]: GM-286's extractor calls it once
//! per file it is handed and sets `container`/`container_parent` on every
//! *declaration* it builds from the answer - never on the file's own `File`
//! node, which is not a container member (see `crate::extractor`'s module
//! doc, and `core::graph::containers`' membership rule: any node with
//! `container` set is treated as one, so a `File` node carrying one would
//! wrongly grow that container's `memberCount`).
//!
//! # What is modeled (Decision 4)
//!
//! - `[package]` - the crate's own name.
//! - `[lib] path`, `[[bin]] path` (or, absent, the `src/lib.rs`/`src/main.rs`
//!   defaults) - crate roots. Every target a package declares becomes its own
//!   crate (see "One package, several crates" below), because that is what
//!   `cargo` itself compiles them as.
//! - `[workspace] members` (with a single-`*`-per-segment glob - see
//!   `cargo_manifest::resolve_globs`) and `exclude`.
//! - The `mod`/`#[path]` module tree - `module_tree`'s whole job.
//!
//! **Not modeled**: `src/bin/*.rs` auto-discovered binaries (Cargo's
//! "autobins" convention - every additional binary this task requires is an
//! explicit `[[bin]]`, and the auto-discovered case is deferred rather than
//! silently wrong: a project relying on it under-reports its bin crates
//! rather than mis-attributing them), and a path dependency outside the
//! workspace (`{ path = "../other" }` under `[dependencies]`). The latter is
//! a real crate `cargo` builds, but modeling it would mean parsing arbitrarily
//! many `Cargo.toml` files outside the walked project - potentially outside
//! the project root entirely - to a resolution depth this plugin has no
//! natural stopping rule for (a path dependency can itself have path
//! dependencies). A workspace member is different: it is *inside* the
//! project by construction, and `members`/`exclude` name it exactly. Neither
//! omission produces a wrong answer for a file this plugin walks at all - a
//! `.rs` file belonging to an unmodeled crate is indexed as an orphan (see
//! below), same as any other file this project model cannot place, not
//! silently dropped.
//!
//! ## One package, several crates
//!
//! A package with both `src/lib.rs` and `src/main.rs` compiles as *two*
//! separate crates - `cargo` does not share a module tree between a
//! package's library and its binaries, and neither does this model: each
//! target ([`resolve_targets`]) becomes its own [`Crate`] with its own
//! [`module_tree::scan_crate`] walk. Their default names can collide (both
//! default to the package's own name when neither gives an explicit `[lib]
//! name`/`[[bin]] name`) - real `cargo` tells them apart by *target kind*,
//! which the wire's container key has no room for (Decision 6: a container
//! key is `<crate>::<module path>`, one flat namespace). This model resolves
//! the collision the same deterministic way it resolves every other one in
//! this file: **first registered wins** (`ProjectContext::load`'s
//! `used_keys`), library before binaries, in manifest-declared order among
//! binaries - and the loser is recorded in [`ProjectContext::notes`], never
//! silently merged into the winner's container.
//!
//! # Decision 5: the orphan container's key
//!
//! A `.rs` file this plugin walks (any project file with a `.rs` extension -
//! that is what the SDK's own `walk_project` hands `extract` regardless of
//! whether this model reached it) that no crate's module tree reaches is
//! still indexed, per the task: "under a synthetic container named after the
//! file path". [`ProjectContext::container_for`] answers
//! [`ContainerInfo::Orphan`] with key `"orphan:<path>"` - `orphan:src/dead.rs`,
//! say. That string can never collide with a real `<crate>::<module path>`
//! key: a real key is one or more `::`-joined Rust identifiers, which by
//! definition contain neither `/` nor `.` nor a lone `:` - and a project-
//! relative path always contains at least one of the first two (a bare
//! filename still has the `.rs` extension). The `orphan:` prefix is
//! redundant with that guarantee on its own, but it is what makes the key
//! legible in a debugger or a `g-mesh plugins check` report without having
//! to already know the convention. GM-286 is expected to carry this same
//! prefix into its declaration nodes' own `nativeKind` as the task's "note in
//! the node's nativeKind" - `crate::extractor`'s module doc names the exact
//! seam (`container_for`'s `Orphan` case) where that information is already
//! available to it; this task's own extractor stub never reaches that code
//! path because it emits no declarations to mark.
//!
//! # Decision 6: crate name normalization and the crate-root key
//!
//! A crate's own key is [`normalize_crate_name`] applied to its target name -
//! `-` becomes `_`, exactly `cargo`'s own rule for turning a package name
//! into the identifier real Rust code refers to it by (`this-crate` compiles
//! as `extern crate this_crate`). The crate root itself - `src/lib.rs` or
//! `src/main.rs` - is registered as a member of container key `<crate>`
//! (bare, no `::`) with **no parent**, matching the design doc's table
//! ("Rust... crate root has none"). That the root is *itself* always a
//! registered container - not merely a prefix other keys happen to share -
//! is what keeps `core::graph::containers::parent_chain` complete enough for
//! `pub(crate)` visibility to resolve once GM-286 emits declarations: that
//! function's own doc warns that an ancestor with no members of its own opens
//! a gap in the parent chain, and closing it requires "emitting each `mod
//! child;` item as a member of the module that declares it" so "a module
//! with submodules then always has a member" - GM-286's task, using the
//! `key`/`parent` this module already computes for the file the `mod
//! child;` line lives in.

mod cargo_manifest;
mod module_tree;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use g_mesh_plugin_sdk::RelPath;

use cargo_manifest::{RawCargoToml, RawTarget};

/// One compiled Rust crate this project model found - a package's library
/// target, or one of its binaries. See this module's doc, "One package,
/// several crates".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crate {
    /// The normalized container key of this crate's own root - see Decision
    /// 6.
    pub key: String,
    /// This crate's root file (`src/lib.rs`, `src/main.rs`, or an explicit
    /// `[lib]`/`[[bin]] path`), project-relative.
    pub root: RelPath,
    /// The directory this crate's own `Cargo.toml` was read from,
    /// project-relative (`""` for the workspace root itself).
    pub package_dir: RelPath,
}

/// What [`ProjectContext::container_for`] answers about one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerInfo {
    /// `path` is reachable from a crate root by a `mod`/`#[path]` chain.
    /// `parent` is `None` only for a crate root itself.
    Member { key: String, parent: Option<String> },
    /// `path` is a `.rs` file no crate's module tree reaches - see this
    /// module's doc, Decision 5. `key` is always `"orphan:<path>"` for this
    /// exact `path`, so two orphan files never share a container.
    Orphan { key: String },
}

/// The Rust plugin's whole project model: every crate this workspace/package
/// declares, and which container owns each file reachable from one of them.
/// See this module's doc for what is and is not modeled, and
/// `crate::extractor`'s module doc for how GM-286 is expected to consume it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectContext {
    crates: Vec<Crate>,
    files: module_tree::FileContainers,
    /// Everything this load could not honestly resolve - a manifest that
    /// would not parse, a crate-key collision, a `mod` naming nothing on
    /// disk, two `mod` items claiming one file. Never fatal (see `load`'s own
    /// doc): a project model with notes is still a complete, deterministic
    /// answer for everything it *could* resolve. Surfaced for diagnostics
    /// (a plugin author's `eprintln`, or a test asserting a specific note
    /// appeared) rather than acted on by this crate itself.
    notes: Vec<String>,
}

impl ProjectContext {
    /// Builds the project model for the project at `root` (an absolute path,
    /// as [`Extractor::load_project`](g_mesh_plugin_sdk::Extractor::load_project)
    /// hands it). Called once by `--bulk-index`, and again on every
    /// `workspaceChanged` - see `crate::main`'s module doc and the SDK's own
    /// `run::Session::load_project`, which is what actually calls this again
    /// after a `Cargo.toml` edit; nothing in this module watches the
    /// filesystem itself.
    ///
    /// Never fails on a bad or missing manifest - a workspace root with no
    /// `Cargo.toml` at all (mid-checkout, or a stray `.rs` file with no
    /// project around it yet) yields an empty model, under which every file
    /// is an orphan (Decision 5), which is a complete and honest answer, not
    /// a degraded one. Only a genuinely unexpected condition (none arise in
    /// this implementation) would propagate as `Err`, per
    /// [`Extractor::load_project`]'s own contract that an `Err` here aborts
    /// `--bulk-index` outright - which this module deliberately tries hard
    /// not to need, since "no crates found" is still indexable.
    pub fn load(root: &Path) -> anyhow::Result<Self> {
        let mut notes = Vec::new();
        let root_manifest_path = root.join("Cargo.toml");
        let root_manifest = match cargo_manifest::read(&root_manifest_path) {
            Ok(manifest) => Some(manifest),
            Err(err) => {
                notes.push(format!(
                    "no usable root Cargo.toml at {}: {err:#} - every .rs file will be indexed as an orphan",
                    root_manifest_path.display()
                ));
                None
            }
        };

        let package_dirs = collect_package_dirs(root, root_manifest.as_ref());

        let mut crates = Vec::new();
        let mut files = module_tree::FileContainers::new();
        let mut used_keys: BTreeSet<String> = BTreeSet::new();

        for package_dir in &package_dirs {
            let manifest = if package_dir == root {
                root_manifest.clone()
            } else {
                match cargo_manifest::read(&package_dir.join("Cargo.toml")) {
                    Ok(manifest) => Some(manifest),
                    Err(err) => {
                        notes.push(format!(
                            "{}: {err:#} - not modeled as a crate",
                            package_dir.join("Cargo.toml").display()
                        ));
                        None
                    }
                }
            };
            let Some(manifest) = manifest else { continue };
            let Some(package) = &manifest.package else {
                notes.push(format!("{} has no [package] - not modeled as a crate", package_dir.display()));
                continue;
            };
            let package_rel_dir = RelPath::relative_to(root, package_dir).unwrap_or_else(|| RelPath::new(""));

            for (key, root_file) in resolve_targets(root, package_dir, &package.name, &manifest) {
                if !used_keys.insert(key.clone()) {
                    notes.push(format!(
                        "crate key {key:?} ({root_file}, package {package_rel_dir}) collides with an \
                         earlier crate of the same name - keeping the first"
                    ));
                    continue;
                }
                module_tree::scan_crate(root, &key, root_file.clone(), &mut files, &mut notes);
                crates.push(Crate { key, root: root_file, package_dir: package_rel_dir.clone() });
            }
        }

        Ok(Self { crates, files, notes })
    }

    /// Every crate this project model found, in the order their `Cargo.toml`
    /// was resolved (workspace root first when it is itself a package, then
    /// `members` in the order `[workspace] members` names or globs them).
    pub fn crates(&self) -> &[Crate] {
        &self.crates
    }

    /// The container `path` belongs to - see [`ContainerInfo`]. Always
    /// answers something: every `.rs` file is either a member of a crate's
    /// module tree or an orphan, never neither.
    pub fn container_for(&self, path: &RelPath) -> ContainerInfo {
        match self.files.get(path) {
            Some((key, parent)) => ContainerInfo::Member { key: key.clone(), parent: parent.clone() },
            None => ContainerInfo::Orphan { key: orphan_key(path) },
        }
    }

    /// Everything this load could not honestly resolve - see this struct's
    /// own field doc.
    pub fn notes(&self) -> &[String] {
        &self.notes
    }
}

/// The synthetic container key for an unreachable file - see this module's
/// doc, Decision 5, for why this exact shape cannot collide with a real
/// `<crate>::<module path>` key.
fn orphan_key(path: &RelPath) -> String {
    format!("orphan:{path}")
}

/// Every package directory this project should look for a crate in: the
/// workspace root itself when its `Cargo.toml` carries `[package]`
/// (a "mixed" manifest), plus every `[workspace] members` glob match not
/// removed by `exclude` - see `cargo_manifest::resolve_globs` for the glob
/// dialect. A manifest with neither `[package]` nor `[workspace]` (or none at
/// all) yields an empty list, which is `load`'s "no crates found, everything
/// is an orphan" case.
fn collect_package_dirs(root: &Path, root_manifest: Option<&RawCargoToml>) -> Vec<PathBuf> {
    let mut package_dirs = Vec::new();
    let Some(manifest) = root_manifest else { return package_dirs };

    if manifest.package.is_some() {
        package_dirs.push(root.to_path_buf());
    }
    if let Some(workspace) = &manifest.workspace {
        let members = cargo_manifest::resolve_globs(root, &workspace.members);
        let excluded = cargo_manifest::resolve_globs(root, &workspace.exclude);
        for member in members {
            if excluded.contains(&member) || package_dirs.contains(&member) {
                continue;
            }
            package_dirs.push(member);
        }
    }
    package_dirs
}

/// Every crate `package_dir`'s own package compiles - its `[lib]` target (or
/// the `src/lib.rs` default) and every `[[bin]]` (or the `src/main.rs`
/// default when none are declared) that actually exists on disk. See this
/// module's doc, "One package, several crates", for why a package can yield
/// more than one entry here.
fn resolve_targets(
    root: &Path,
    package_dir: &Path,
    package_name: &str,
    manifest: &RawCargoToml,
) -> Vec<(String, RelPath)> {
    let mut targets = Vec::new();

    let lib_path =
        manifest.lib.as_ref().and_then(|lib| lib.path.clone()).unwrap_or_else(|| "src/lib.rs".into());
    if let Some(root_file) = existing_relpath(root, package_dir, &lib_path) {
        let name =
            manifest.lib.as_ref().and_then(|lib| lib.name.clone()).unwrap_or_else(|| package_name.into());
        targets.push((normalize_crate_name(&name), root_file));
    }

    let mut bins = manifest.bins.clone();
    if bins.is_empty() {
        // Cargo's own default: a single implicit `[[bin]]` at `src/main.rs`,
        // named after the package, when none is declared explicitly. This
        // model does not go further and auto-discover `src/bin/*.rs` - see
        // this module's doc, "What is modeled".
        bins.push(RawTarget::default());
    }
    for bin in &bins {
        let bin_path = bin.path.clone().unwrap_or_else(|| "src/main.rs".into());
        if let Some(root_file) = existing_relpath(root, package_dir, &bin_path) {
            let name = bin.name.clone().unwrap_or_else(|| package_name.into());
            targets.push((normalize_crate_name(&name), root_file));
        }
    }

    targets
}

fn existing_relpath(root: &Path, package_dir: &Path, relative: &str) -> Option<RelPath> {
    let absolute = package_dir.join(relative);
    absolute.is_file().then(|| RelPath::relative_to(root, &absolute)).flatten()
}

/// `cargo`'s own crate-identifier rule: a package or target name's `-`
/// becomes `_`. Applied uniformly to `[package] name`, `[lib] name` and
/// `[[bin]] name` - see this module's doc, Decision 6.
fn normalize_crate_name(name: &str) -> String {
    name.replace('-', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree(PathBuf);

    impl Tree {
        /// A uniquely named scratch directory, removed on drop. Unique per
        /// call - not just per `name` - because `cargo test` runs these
        /// `#[test]` functions concurrently in one process: two tests both
        /// naming their tree `"workspace"` raced on `remove_dir_all` /
        /// `create_dir_all` against the very same path until this carried a
        /// nanosecond timestamp too, and failed with spurious "No such file
        /// or directory" / "Invalid argument" errors that had nothing to do
        /// with this module's own logic.
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            let root = std::env::temp_dir()
                .join(format!("g-mesh-plugin-rust-project-{}-{name}-{nanos}", std::process::id()));
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

        fn remove(&self, path: &str) -> &Self {
            let _ = std::fs::remove_file(self.0.join(path));
            self
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn workspace() -> Tree {
        let tree = Tree::new("workspace");
        tree.write("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/excluded\"]\n");
        tree.write("crates/alpha/Cargo.toml", "[package]\nname = \"alpha\"\n");
        tree.write("crates/alpha/src/lib.rs", "mod nested;\nmod inline_mod {\n    mod deeper {}\n}\n");
        tree.write("crates/alpha/src/nested/mod.rs", "mod deep;\n");
        tree.write("crates/alpha/src/nested/deep.rs", "");
        tree.write("crates/alpha/src/orphan.rs", "");
        tree.write("crates/beta/Cargo.toml", "[package]\nname = \"beta-crate\"\n");
        tree.write("crates/beta/src/main.rs", "");
        tree.write("crates/excluded/Cargo.toml", "[package]\nname = \"excluded\"\n");
        tree.write("crates/excluded/src/lib.rs", "");
        tree
    }

    #[test]
    fn two_member_crates_are_found_and_the_excluded_glob_match_is_not() {
        let tree = workspace();
        let context = ProjectContext::load(&tree.0).unwrap();
        let keys: Vec<&str> = context.crates().iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec!["alpha", "beta_crate"], "{:?}", context.notes());
    }

    #[test]
    fn the_crate_root_is_its_own_container_with_no_parent() {
        let tree = workspace();
        let context = ProjectContext::load(&tree.0).unwrap();
        assert_eq!(
            context.container_for(&RelPath::new("crates/alpha/src/lib.rs")),
            ContainerInfo::Member { key: "alpha".to_string(), parent: None }
        );
    }

    #[test]
    fn a_nested_mod_rs_and_its_own_child_get_the_right_parent_chain() {
        let tree = workspace();
        let context = ProjectContext::load(&tree.0).unwrap();
        assert_eq!(
            context.container_for(&RelPath::new("crates/alpha/src/nested/mod.rs")),
            ContainerInfo::Member { key: "alpha::nested".to_string(), parent: Some("alpha".to_string()) }
        );
        assert_eq!(
            context.container_for(&RelPath::new("crates/alpha/src/nested/deep.rs")),
            ContainerInfo::Member {
                key: "alpha::nested::deep".to_string(),
                parent: Some("alpha::nested".to_string())
            }
        );
    }

    /// Inline modules (including a nested one) do not own a file of their
    /// own - only what a `mod` item's *file* resolves to is a member.
    #[test]
    fn inline_modules_are_not_separately_registered_as_files() {
        let tree = workspace();
        let context = ProjectContext::load(&tree.0).unwrap();
        for absent in ["crates/alpha/src/inline_mod.rs", "crates/alpha/src/inline_mod/deeper.rs"] {
            assert!(
                matches!(context.container_for(&RelPath::new(absent)), ContainerInfo::Orphan { .. }),
                "{absent} is not a real file and must not have been claimed"
            );
        }
    }

    #[test]
    fn a_file_no_crate_reaches_is_an_orphan_named_after_its_own_path() {
        let tree = workspace();
        let context = ProjectContext::load(&tree.0).unwrap();
        assert_eq!(
            context.container_for(&RelPath::new("crates/alpha/src/orphan.rs")),
            ContainerInfo::Orphan { key: "orphan:crates/alpha/src/orphan.rs".to_string() }
        );
    }

    /// Every file under the excluded member is orphaned, its own `lib.rs`
    /// included - `exclude` removes the crate from the model entirely, it
    /// does not just skip enumerating it as a workspace member.
    #[test]
    fn every_file_under_an_excluded_member_is_orphaned() {
        let tree = workspace();
        let context = ProjectContext::load(&tree.0).unwrap();
        assert!(context.crates().iter().all(|c| c.key != "excluded"));
        assert!(matches!(
            context.container_for(&RelPath::new("crates/excluded/src/lib.rs")),
            ContainerInfo::Orphan { .. }
        ));
    }

    /// Decision 4: crate name normalization - `beta-crate`'s package name
    /// becomes container key `beta_crate`.
    #[test]
    fn a_dashed_package_name_is_normalized_to_underscores() {
        let tree = workspace();
        let context = ProjectContext::load(&tree.0).unwrap();
        assert_eq!(
            context.container_for(&RelPath::new("crates/beta/src/main.rs")),
            ContainerInfo::Member { key: "beta_crate".to_string(), parent: None }
        );
    }

    /// Acceptance: editing `Cargo.toml` and reloading (`workspaceChanged`'s
    /// own effect, exercised directly here rather than through the SDK's
    /// control loop - `ProjectContext::load` is exactly what
    /// `run::Session::load_project` calls again) changes the model.
    #[test]
    fn reloading_after_a_cargo_toml_edit_picks_up_the_new_membership() {
        let tree = workspace();
        let before = ProjectContext::load(&tree.0).unwrap();
        assert!(before.crates().iter().any(|c| c.key == "beta_crate"));

        tree.write(
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/excluded\", \"crates/beta\"]\n",
        );
        let after = ProjectContext::load(&tree.0).unwrap();
        assert!(!after.crates().iter().any(|c| c.key == "beta_crate"), "{:?}", after.crates());
        assert!(after.crates().iter().any(|c| c.key == "alpha"), "unrelated membership must be unaffected");
    }

    /// A package with both `src/lib.rs` and `src/main.rs`, neither naming
    /// itself explicitly, produces two crate roots that would collide on the
    /// same default key - the library wins deterministically (Decision 6 /
    /// "One package, several crates"), and the loss is noted rather than
    /// silently merging the two module trees into one container.
    #[test]
    fn a_lib_and_bin_defaulting_to_the_same_name_do_not_merge_their_containers() {
        let tree = Tree::new("lib-and-bin-collide");
        tree.write("Cargo.toml", "[package]\nname = \"tool\"\n");
        tree.write("src/lib.rs", "mod shared;\n");
        tree.write("src/shared.rs", "");
        tree.write("src/main.rs", "mod shared;\n"); // would also claim `tool::shared` if not deduped
        let context = ProjectContext::load(&tree.0).unwrap();
        let keys: Vec<&str> = context.crates().iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec!["tool"], "only the lib crate root registers under the shared key");
        assert_eq!(
            context.container_for(&RelPath::new("src/lib.rs")),
            ContainerInfo::Member { key: "tool".to_string(), parent: None }
        );
        assert!(
            matches!(context.container_for(&RelPath::new("src/main.rs")), ContainerInfo::Orphan { .. }),
            "the losing crate root's own file is not silently absorbed into the winner's tree either"
        );
        assert!(context.notes().iter().any(|note| note.contains("collides")), "{:?}", context.notes());
    }

    #[test]
    fn a_missing_root_cargo_toml_yields_an_empty_model_not_an_error() {
        let tree = Tree::new("no-manifest");
        tree.write("src/lib.rs", "");
        let context = ProjectContext::load(&tree.0).unwrap();
        assert!(context.crates().is_empty());
        assert!(matches!(context.container_for(&RelPath::new("src/lib.rs")), ContainerInfo::Orphan { .. }));
        assert!(!context.notes().is_empty());
    }

    /// A single, non-workspace package (`[package]` with no `[workspace]` at
    /// all) is still one crate, exercising the whole model on the common
    /// case rather than only the workspace one.
    #[test]
    fn a_lone_package_with_no_workspace_table_is_still_one_crate() {
        let tree = Tree::new("lone-package");
        tree.write("Cargo.toml", "[package]\nname = \"solo\"\n");
        tree.write("src/lib.rs", "");
        let context = ProjectContext::load(&tree.0).unwrap();
        let keys: Vec<&str> = context.crates().iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec!["solo"]);
    }

    #[test]
    fn removing_a_member_directory_and_reloading_orphans_its_files() {
        let tree = workspace();
        let context = ProjectContext::load(&tree.0).unwrap();
        assert!(context.crates().iter().any(|c| c.key == "beta_crate"));
        tree.remove("crates/beta/Cargo.toml");
        let reloaded = ProjectContext::load(&tree.0).unwrap();
        assert!(!reloaded.crates().iter().any(|c| c.key == "beta_crate"));
        assert!(matches!(
            reloaded.container_for(&RelPath::new("crates/beta/src/main.rs")),
            ContainerInfo::Orphan { .. }
        ));
    }
}
