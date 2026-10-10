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
pub mod facts;
mod module_tree;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use g_mesh_plugin_sdk::RelPath;

use cargo_manifest::{RawCargoToml, RawTarget};

/// Directories the walk never descends into, beyond
/// [`g_mesh_plugin_sdk::BASELINE_EXCLUDED_DIRS`]: cargo's own build output,
/// never source.
///
/// This constant is the one list in code: `main.rs`'s
/// [`g_mesh_plugin_sdk::PluginSpec::exclude_dirs`] fallback, the census and
/// the tests all take it from here. The one copy that cannot be derived is
/// `plugin.toml`'s `[plugin.workspace] exclude_dirs`, which core, its watcher
/// and `status` read before any plugin process exists; the test
/// `plugin_toml_exclude_dirs_equal_exclude_dirs` below pins the two equal, so
/// editing either one alone fails the build's tests.
pub const EXCLUDE_DIRS: [&str; 1] = ["target"];

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
    /// The absolute project root the model was loaded from, which a re-scan
    /// reads again.
    root: PathBuf,
    crates: Vec<Crate>,
    /// Each crate's own module tree, index-aligned with `crates`. `files`,
    /// `container_keys` and `notes` are derived from them by
    /// [`ProjectContext::rebuild`].
    trees: Vec<module_tree::CrateTree>,
    /// The order of `notes`: load-time notes, and where each crate's scan
    /// notes fall among them.
    note_layout: Vec<NoteSlot>,
    files: module_tree::FileContainers,
    /// Every container key `files` (and each crate root) actually contains -
    /// the reverse of `files`, kept as its own set rather than recomputed on
    /// every query. This is what [`ProjectContext::has_container`] answers
    /// from; see that method's own doc for why the extractor needs it (GM-358).
    container_keys: BTreeSet<String>,
    /// Every crate-root path a loaded manifest names (an explicit `[lib]`/
    /// `[[bin]] path`, or the `src/lib.rs`/`src/main.rs` default), mapped to
    /// whether it was a file at load. A root whose existence flips is what
    /// [`ProjectContext::source_changed`] answers with a reload.
    root_files: BTreeMap<RelPath, bool>,
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
        let mut notes = NoteLayout::default();
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
        let mut trees: Vec<module_tree::CrateTree> = Vec::new();
        let mut taken: BTreeSet<RelPath> = BTreeSet::new();
        let mut used_keys: BTreeSet<String> = BTreeSet::new();
        let mut root_files: BTreeMap<RelPath, bool> = BTreeMap::new();

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

            for Target { key, root_file, exists } in
                resolve_targets(root, package_dir, &package.name, &manifest)
            {
                let seen = root_files.entry(root_file.clone()).or_insert(false);
                *seen |= exists;
                if !exists {
                    continue;
                }
                if !used_keys.insert(key.clone()) {
                    notes.push(format!(
                        "crate key {key:?} ({root_file}, package {package_rel_dir}) collides with an \
                         earlier crate of the same name - keeping the first"
                    ));
                    continue;
                }
                let tree = module_tree::scan_crate(root, &key, root_file.clone(), &taken);
                taken.extend(tree.files.keys().cloned());
                notes.0.push(NoteSlot::Crate(trees.len()));
                trees.push(tree);
                crates.push(Crate { key, root: root_file, package_dir: package_rel_dir.clone() });
            }
        }

        let mut project = Self {
            root: root.to_path_buf(),
            crates,
            trees,
            note_layout: notes.0,
            root_files,
            ..Self::default()
        };
        project.rebuild();
        Ok(project)
    }

    /// Derives `files`, `container_keys` and `notes` from `trees` and
    /// `note_layout`.
    fn rebuild(&mut self) {
        self.files = self.trees.iter().flat_map(|tree| tree.files.clone()).collect();
        self.container_keys = self.files.values().map(|(key, _)| key.clone()).collect();
        self.notes = self
            .note_layout
            .iter()
            .flat_map(|slot| match slot {
                NoteSlot::Text(note) => std::slice::from_ref(note),
                NoteSlot::Crate(index) => self.trees[*index].notes.as_slice(),
            })
            .cloned()
            .collect();
    }

    /// Updates the module tree for `path`'s new text (`None`: gone or
    /// unreadable) and answers which other files may now extract
    /// differently, or `None` when no other file does.
    ///
    /// - A crate-root path a manifest names that appeared (text given, absent
    ///   at load) or disappeared (`None`, present at load): the whole model is
    ///   reloaded, as `cargo` discovers a default root without a manifest
    ///   edit.
    /// - A file of the tree whose `mod` signature is unchanged: `None`,
    ///   without reading the disk.
    /// - A file of the tree with another signature, or gone: its crate is
    ///   re-scanned.
    /// - A file outside the tree that a `mod` item named before it existed
    ///   (a pending candidate): every crate naming it is re-scanned.
    /// - Anything else: `None`.
    ///
    /// A re-scan reads the disk, so afterwards the model equals
    /// [`ProjectContext::load`]'s for the same module files; a reload is
    /// [`ProjectContext::load`] itself. The delta is
    /// [`facts::delta`] of the model before and after, without `path` itself,
    /// which the caller extracts anyway.
    pub fn source_changed(
        &mut self,
        path: &RelPath,
        source: Option<&str>,
    ) -> Option<g_mesh_plugin_sdk::wire::ResolutionDelta> {
        if self.root_files.get(path) == Some(&source.is_none()) {
            return self.reload_for(path);
        }

        let owner = self.trees.iter().position(|tree| tree.files.contains_key(path));
        let forced: BTreeSet<usize> = match (owner, source) {
            (Some(owner), Some(text)) => {
                let signature = self.trees[owner].signatures.get(path);
                if signature == Some(&module_tree::mod_signature(text)) {
                    return None;
                }
                BTreeSet::from([owner])
            }
            (Some(owner), None) => BTreeSet::from([owner]),
            (None, Some(_)) => {
                (0..self.trees.len()).filter(|&index| self.trees[index].pending.contains(path)).collect()
            }
            (None, None) => BTreeSet::new(),
        };
        if forced.is_empty() {
            return None;
        }

        let before = facts::RustFacts::of(self);
        self.rescan(&forced);
        self.delta_since(&before, path)
    }

    /// Replaces the model with a fresh [`ProjectContext::load`] of the same
    /// root and answers what changed, as [`ProjectContext::source_changed`]
    /// does. A load that fails keeps the model and answers `Unknown`.
    fn reload_for(&mut self, path: &RelPath) -> Option<g_mesh_plugin_sdk::wire::ResolutionDelta> {
        let before = facts::RustFacts::of(self);
        match Self::load(&self.root) {
            Ok(fresh) => *self = fresh,
            Err(err) => {
                return Some(g_mesh_plugin_sdk::wire::ResolutionDelta::Unknown {
                    reason: format!("reloading the project after {path} changed failed: {err:#}"),
                })
            }
        }
        self.delta_since(&before, path)
    }

    /// [`facts::delta`] from `before` to the current model, without `path`
    /// itself, which the caller extracts anyway; `None` when nothing else
    /// changed.
    fn delta_since(
        &self,
        before: &facts::RustFacts,
        path: &RelPath,
    ) -> Option<g_mesh_plugin_sdk::wire::ResolutionDelta> {
        use g_mesh_plugin_sdk::wire::ResolutionDelta;

        let after = facts::RustFacts::of(self);
        match facts::delta(before, &after) {
            ResolutionDelta::Unchanged => None,
            ResolutionDelta::Affected { mut files, imports } => {
                files.retain(|scope| scope.under != path.as_str());
                if files.is_empty() && imports.is_empty() {
                    None
                } else {
                    Some(ResolutionDelta::Affected { files, imports })
                }
            }
            unknown @ ResolutionDelta::Unknown { .. } => Some(unknown),
        }
    }

    /// Re-scans every crate in `forced`, then every later crate whose own
    /// scan the change can reach: one that claimed, lost or is waiting for a
    /// file whose claim moved. A crate's scan depends only on the disk and
    /// on the files earlier crates claimed, so crates before the first
    /// forced one are kept as they are.
    fn rescan(&mut self, forced: &BTreeSet<usize>) {
        let Some(&first) = forced.first() else { return };
        let mut taken: BTreeSet<RelPath> =
            self.trees[..first].iter().flat_map(|tree| tree.files.keys().cloned()).collect();
        let mut moved: BTreeSet<RelPath> = BTreeSet::new();
        for index in first..self.trees.len() {
            let old = &self.trees[index];
            let reached = |path: &RelPath| {
                old.files.contains_key(path) || old.lost.contains(path) || old.pending.contains(path)
            };
            if forced.contains(&index) || moved.iter().any(reached) {
                let krate = &self.crates[index];
                let new = module_tree::scan_crate(&self.root, &krate.key, krate.root.clone(), &taken);
                let old_claims: BTreeSet<&RelPath> = old.files.keys().collect();
                let new_claims: BTreeSet<&RelPath> = new.files.keys().collect();
                moved.extend(old_claims.symmetric_difference(&new_claims).map(|path| (*path).clone()));
                self.trees[index] = new;
            }
            taken.extend(self.trees[index].files.keys().cloned());
        }
        self.rebuild();
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

    /// Whether `key` names a module this project's own module tree actually
    /// contains - `plugins/python`'s analogous `has_container` (GM-358) is
    /// the sibling of this method, and the reason both exist is the same
    /// shape of gap: `use crate::a::b;`'s leaf, `b`, may itself be a
    /// submodule of `a` rather than a symbol declared inside it, and only a
    /// whole-crate registry - not the resolving file's own `FileModel`,
    /// which knows only what *that file* declares - can tell the two apart.
    /// `resolve_module_path` cannot answer this either: past the first
    /// segment it appends every further one blindly (its own doc says why),
    /// so `a::b` being syntactically well-formed says nothing about whether
    /// `b` is real.
    ///
    /// Answered from `files`, the same source [`container_for`](Self::container_for)
    /// reads - so it inherits that source's one gap: an **inline** `mod b {
    /// … }` has no file of its own and is not in `files` (see
    /// `module_tree`'s own doc and its
    /// `a_file_mod_and_an_inline_mod_both_get_the_right_key` test), so this
    /// answers `false` for an inline submodule that is genuinely there. That
    /// is a missing edge, never a wrong one - the same trade-off
    /// `FileModel::lookup_name` documents for an ambiguous name.
    pub fn has_container(&self, key: &str) -> bool {
        self.container_keys.contains(key)
    }

    /// Everything this load could not honestly resolve - see this struct's
    /// own field doc.
    pub fn notes(&self) -> &[String] {
        &self.notes
    }
}

/// One entry of [`ProjectContext`]'s `note_layout`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NoteSlot {
    /// A note of the manifest pass.
    Text(String),
    /// The scan notes of the crate at this index.
    Crate(usize),
}

/// `load`'s notes as it records them.
#[derive(Default)]
struct NoteLayout(Vec<NoteSlot>);

impl NoteLayout {
    fn push(&mut self, note: String) {
        self.0.push(NoteSlot::Text(note));
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

/// Every crate root `package_dir`'s own package names - its `[lib]` target
/// (or the `src/lib.rs` default) and every `[[bin]]` (or the `src/main.rs`
/// default when none are declared), each marked with whether it exists on
/// disk; only an existing one is compiled as a crate. See this
/// module's doc, "One package, several crates", for why a package can yield
/// more than one entry here.
fn resolve_targets(
    root: &Path,
    package_dir: &Path,
    package_name: &str,
    manifest: &RawCargoToml,
) -> Vec<Target> {
    let mut targets = Vec::new();
    let mut push = |name: String, relative: &str| {
        let absolute = package_dir.join(relative);
        if let Some(root_file) = RelPath::relative_to(root, &absolute) {
            targets.push(Target { key: normalize_crate_name(&name), root_file, exists: absolute.is_file() });
        }
    };

    let lib_path =
        manifest.lib.as_ref().and_then(|lib| lib.path.clone()).unwrap_or_else(|| "src/lib.rs".into());
    let lib_name =
        manifest.lib.as_ref().and_then(|lib| lib.name.clone()).unwrap_or_else(|| package_name.into());
    push(lib_name, &lib_path);

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
        push(bin.name.clone().unwrap_or_else(|| package_name.into()), &bin_path);
    }

    targets
}

/// One crate root a package's manifest names: its normalized crate key, its
/// project-relative path, and whether that path is a file now.
struct Target {
    key: String,
    root_file: RelPath,
    exists: bool,
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

    /// `plugin.toml` cannot be derived from [`EXCLUDE_DIRS`] (core reads it
    /// as data), so it is pinned here instead. Compared exactly, order
    /// included: both are hand-written lists, and "copy it verbatim" is the
    /// simplest rule to follow.
    #[test]
    fn plugin_toml_exclude_dirs_equal_exclude_dirs() {
        let manifest: toml::Value =
            toml::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml"))).unwrap();
        let listed: Vec<&str> = manifest["plugin"]["workspace"]["exclude_dirs"]
            .as_array()
            .expect("plugin.toml declares [plugin.workspace] exclude_dirs")
            .iter()
            .map(|dir| dir.as_str().expect("every exclude_dirs entry is a string"))
            .collect();
        assert_eq!(listed, EXCLUDE_DIRS, "plugin.toml's exclude_dirs must equal project::EXCLUDE_DIRS");
    }

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

    // --- GM-507: a source save re-scans the module tree -------------------
    //
    // `ProjectContext::source_changed`. Design:
    // `docs/architecture/gm-507-rust-module-tree-refresh.md`, section 5.

    use g_mesh_plugin_sdk::wire::ResolutionDelta;

    /// One package `alpha` whose `lib.rs` declares `a`.
    fn alpha() -> Tree {
        let tree = Tree::new("source-changed");
        tree.write("Cargo.toml", "[package]\nname = \"alpha\"\n");
        tree.write("src/lib.rs", "pub mod a;\n");
        tree.write("src/a.rs", "pub fn f() {}\n");
        tree
    }

    /// Writes `text` to `path` and hands it to the model, as the SDK does on
    /// that file's `fileChanged`.
    fn save(context: &mut ProjectContext, tree: &Tree, path: &str, text: &str) -> Option<ResolutionDelta> {
        tree.write(path, text);
        context.source_changed(&RelPath::new(path), Some(text))
    }

    /// Deletes `path` and tells the model, as the SDK does.
    fn delete(context: &mut ProjectContext, tree: &Tree, path: &str) -> Option<ResolutionDelta> {
        tree.remove(path);
        context.source_changed(&RelPath::new(path), None)
    }

    /// The file scopes of an `Affected` delta.
    fn scoped(delta: &Option<ResolutionDelta>) -> Vec<String> {
        match delta {
            Some(ResolutionDelta::Affected { files, .. }) => {
                files.iter().map(|scope| scope.under.clone()).collect()
            }
            other => panic!("expected an affected delta, got {other:?}"),
        }
    }

    fn key_of(context: &ProjectContext, path: &str) -> String {
        match context.container_for(&RelPath::new(path)) {
            ContainerInfo::Member { key, .. } | ContainerInfo::Orphan { key } => key,
        }
    }

    /// Behaviour 6: the updated model is the one a cold `load` builds.
    fn assert_equals_load(context: &ProjectContext, tree: &Tree) {
        assert_eq!(
            *context,
            ProjectContext::load(&tree.0).unwrap(),
            "the updated model differs from a cold load"
        );
    }

    /// Behaviour 1: the child file exists first (an orphan); the parent's
    /// save adding `mod child;` places it and names it in the delta, without
    /// naming the saved file itself.
    ///
    /// Control: answer `None` when the signature differs (the `(Some(owner),
    /// Some(text))` arm returns before `rescan`).
    #[test]
    fn a_child_file_then_its_mod_line_places_the_child() {
        let tree = alpha();
        let mut context = ProjectContext::load(&tree.0).unwrap();
        tree.write("src/child.rs", "pub fn c() {}\n");
        assert_eq!(context.source_changed(&RelPath::new("src/child.rs"), Some("pub fn c() {}\n")), None);
        assert_eq!(key_of(&context, "src/child.rs"), "orphan:src/child.rs");

        let delta = save(&mut context, &tree, "src/lib.rs", "pub mod a;\npub mod child;\n");

        assert_eq!(
            context.container_for(&RelPath::new("src/child.rs")),
            ContainerInfo::Member { key: "alpha::child".to_string(), parent: Some("alpha".to_string()) }
        );
        let files = scoped(&delta);
        assert!(files.contains(&"src/child.rs".to_string()), "{delta:?}");
        assert!(!files.contains(&"src/lib.rs".to_string()), "the saved file is not named: {delta:?}");
        assert_equals_load(&context, &tree);
    }

    /// Behaviour 2: `mod child;` first (naming nothing yet), the file
    /// second: the file's own save places it. The delta does not name the
    /// file itself, which its own round trip extracts.
    ///
    /// Control: answer an empty `forced` set in the `(None, Some(_))` arm of
    /// `source_changed` (the pending candidate is never re-scanned).
    #[test]
    fn a_mod_line_then_its_child_file_places_the_child() {
        let tree = alpha();
        let mut context = ProjectContext::load(&tree.0).unwrap();
        save(&mut context, &tree, "src/lib.rs", "pub mod a;\npub mod child;\n");
        assert_eq!(key_of(&context, "src/child.rs"), "orphan:src/child.rs", "nothing on disk yet");

        let delta = save(&mut context, &tree, "src/child.rs", "pub fn c() {}\n");

        assert_eq!(key_of(&context, "src/child.rs"), "alpha::child");
        if delta.is_some() {
            assert!(!scoped(&delta).contains(&"src/child.rs".to_string()), "{delta:?}");
        }
        assert_equals_load(&context, &tree);
    }

    /// Behaviour 3: removing the `mod` item orphans the child and its own
    /// child, and names both.
    ///
    /// Control: as for behaviour 1.
    #[test]
    fn removing_the_mod_item_orphans_the_child_subtree() {
        let tree = alpha();
        tree.write("src/lib.rs", "pub mod a;\npub mod child;\n");
        tree.write("src/child.rs", "pub mod grand;\n");
        tree.write("src/child/grand.rs", "pub fn g() {}\n");
        let mut context = ProjectContext::load(&tree.0).unwrap();
        assert_eq!(key_of(&context, "src/child/grand.rs"), "alpha::child::grand");

        let delta = save(&mut context, &tree, "src/lib.rs", "pub mod a;\n");

        assert_eq!(key_of(&context, "src/child.rs"), "orphan:src/child.rs");
        assert_eq!(key_of(&context, "src/child/grand.rs"), "orphan:src/child/grand.rs");
        let files = scoped(&delta);
        for path in ["src/child.rs", "src/child/grand.rs"] {
            assert!(files.contains(&path.to_string()), "{path}: {delta:?}");
        }
        assert_equals_load(&context, &tree);
    }

    /// Behaviour 4: deleting a module file orphans what it declared, and
    /// names it.
    ///
    /// Control: answer `None` in the `(Some(owner), None)` arm of
    /// `source_changed`.
    #[test]
    fn deleting_a_module_file_orphans_its_children() {
        let tree = alpha();
        tree.write("src/lib.rs", "pub mod a;\npub mod child;\n");
        tree.write("src/child.rs", "pub mod grand;\n");
        tree.write("src/child/grand.rs", "pub fn g() {}\n");
        let mut context = ProjectContext::load(&tree.0).unwrap();

        let delta = delete(&mut context, &tree, "src/child.rs");

        assert_eq!(key_of(&context, "src/child/grand.rs"), "orphan:src/child/grand.rs");
        assert!(scoped(&delta).contains(&"src/child/grand.rs".to_string()), "{delta:?}");
        assert_equals_load(&context, &tree);
    }

    /// Behaviour 5: a save keeping every `mod` item answers `None` from the
    /// text alone. The disk here already holds a new `mod child;` and its
    /// file; the model does not read it.
    ///
    /// Control: drop the signature comparison in the `(Some(owner),
    /// Some(text))` arm (the disk is re-scanned and `child` placed).
    #[test]
    fn a_save_keeping_the_mod_items_changes_nothing_and_reads_no_disk() {
        let tree = alpha();
        let mut context = ProjectContext::load(&tree.0).unwrap();
        let before = context.clone();
        tree.write("src/lib.rs", "pub mod a;\npub mod child;\n");
        tree.write("src/child.rs", "");

        let delta = context
            .source_changed(&RelPath::new("src/lib.rs"), Some("pub mod a;\n// edited\npub fn x() {}\n"));

        assert_eq!(delta, None);
        assert_eq!(context, before, "the model is untouched");
    }

    /// Owner decision after S5: a re-scan reaches a later crate sharing a
    /// file the earlier one claimed. `lib.rs` (`toollib`) and `main.rs`
    /// (`tool`) both declare `mod util;`; the lib claims it. Removing the
    /// lib's `mod util;` hands `util.rs` to the bin, and restoring it hands it
    /// back, each time equal to a cold load.
    ///
    /// Control: re-scan only `forced` crates in `ProjectContext::rescan`
    /// (drop `|| moved.iter().any(reached)`): `util.rs` stays an orphan.
    #[test]
    fn a_later_crate_sharing_a_file_is_re_scanned_too() {
        let tree = Tree::new("shared-file");
        tree.write("Cargo.toml", "[package]\nname = \"tool\"\n\n[lib]\nname = \"toollib\"\n");
        tree.write("src/lib.rs", "mod util;\n");
        tree.write("src/main.rs", "mod util;\n");
        tree.write("src/util.rs", "pub fn u() {}\n");
        let mut context = ProjectContext::load(&tree.0).unwrap();
        let keys: Vec<&str> = context.crates().iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec!["toollib", "tool"]);
        assert_eq!(key_of(&context, "src/util.rs"), "toollib::util");

        let delta = save(&mut context, &tree, "src/lib.rs", "");
        assert_eq!(key_of(&context, "src/util.rs"), "tool::util");
        assert!(scoped(&delta).contains(&"src/util.rs".to_string()), "{delta:?}");
        assert_equals_load(&context, &tree);

        save(&mut context, &tree, "src/lib.rs", "mod util;\n");
        assert_eq!(key_of(&context, "src/util.rs"), "toollib::util");
        assert_equals_load(&context, &tree);
    }

    // --- A crate root file created or deleted reloads the model ----------

    fn crate_keys(context: &ProjectContext) -> Vec<&str> {
        context.crates().iter().map(|c| c.key.as_str()).collect()
    }

    fn is_unknown(delta: &Option<ResolutionDelta>) -> bool {
        matches!(delta, Some(ResolutionDelta::Unknown { .. }))
    }

    /// A package with no crate root yet: creating the default `src/lib.rs`
    /// adds the crate and places its `mod` children, answering `Unknown`
    /// (the crate names changed); deleting it drops the crate again, with no
    /// note left behind. Each step equals a cold load.
    #[test]
    fn creating_and_deleting_src_lib_rs_adds_and_drops_the_crate() {
        let tree = Tree::new("lib-root-created");
        tree.write("Cargo.toml", "[package]\nname = \"alpha\"\n");
        tree.write("src/a.rs", "pub fn f() {}\n");
        let mut context = ProjectContext::load(&tree.0).unwrap();
        assert!(context.crates().is_empty());
        assert_eq!(key_of(&context, "src/a.rs"), "orphan:src/a.rs");

        let delta = save(&mut context, &tree, "src/lib.rs", "pub mod a;\n");
        assert!(is_unknown(&delta), "{delta:?}");
        assert_eq!(crate_keys(&context), vec!["alpha"]);
        assert_eq!(
            context.container_for(&RelPath::new("src/a.rs")),
            ContainerInfo::Member { key: "alpha::a".to_string(), parent: Some("alpha".to_string()) }
        );
        assert_equals_load(&context, &tree);

        let delta = delete(&mut context, &tree, "src/lib.rs");
        assert!(is_unknown(&delta), "{delta:?}");
        assert!(context.crates().is_empty(), "{:?}", context.crates());
        assert_eq!(key_of(&context, "src/a.rs"), "orphan:src/a.rs");
        assert!(context.notes().is_empty(), "{:?}", context.notes());
        assert_equals_load(&context, &tree);
    }

    /// The bin default: a package whose lib is named apart gains the
    /// `src/main.rs` crate when that file is created, and loses it on
    /// deletion, each step equal to a cold load.
    #[test]
    fn creating_and_deleting_src_main_rs_adds_and_drops_the_bin_crate() {
        let tree = Tree::new("bin-root-created");
        tree.write("Cargo.toml", "[package]\nname = \"alpha\"\n\n[lib]\nname = \"alphalib\"\n");
        tree.write("src/lib.rs", "");
        tree.write("src/cli.rs", "pub fn run() {}\n");
        let mut context = ProjectContext::load(&tree.0).unwrap();
        assert_eq!(crate_keys(&context), vec!["alphalib"]);

        let delta = save(&mut context, &tree, "src/main.rs", "mod cli;\nfn main() {}\n");
        assert!(is_unknown(&delta), "{delta:?}");
        assert_eq!(crate_keys(&context), vec!["alphalib", "alpha"]);
        assert_eq!(key_of(&context, "src/cli.rs"), "alpha::cli");
        assert_equals_load(&context, &tree);

        let delta = delete(&mut context, &tree, "src/main.rs");
        assert!(is_unknown(&delta), "{delta:?}");
        assert_eq!(crate_keys(&context), vec!["alphalib"]);
        assert_eq!(key_of(&context, "src/cli.rs"), "orphan:src/cli.rs");
        assert!(context.notes().is_empty(), "{:?}", context.notes());
        assert_equals_load(&context, &tree);
    }

    /// `src/lib.rs` and `src/main.rs` share the default key and the lib
    /// wins. Deleting `lib.rs` hands the key to `main.rs`'s crate and
    /// re-creating it takes it back: the crate names stay the same, so each
    /// step answers `Affected` (not `Unknown`) and equals a cold load.
    #[test]
    fn deleting_the_winning_lib_rs_promotes_the_bin_under_the_same_key() {
        let tree = Tree::new("lib-bin-swap");
        tree.write("Cargo.toml", "[package]\nname = \"tool\"\n");
        tree.write("src/lib.rs", "pub mod shared;\n");
        tree.write("src/shared.rs", "");
        tree.write("src/main.rs", "mod cli;\nfn main() {}\n");
        tree.write("src/cli.rs", "");
        let mut context = ProjectContext::load(&tree.0).unwrap();
        assert_eq!(key_of(&context, "src/shared.rs"), "tool::shared");
        assert_eq!(key_of(&context, "src/cli.rs"), "orphan:src/cli.rs");

        let delta = delete(&mut context, &tree, "src/lib.rs");
        assert_eq!(crate_keys(&context), vec!["tool"]);
        assert_eq!(
            context.container_for(&RelPath::new("src/main.rs")),
            ContainerInfo::Member { key: "tool".to_string(), parent: None }
        );
        assert_eq!(key_of(&context, "src/cli.rs"), "tool::cli");
        assert_eq!(key_of(&context, "src/shared.rs"), "orphan:src/shared.rs");
        let files = scoped(&delta);
        for path in ["src/cli.rs", "src/shared.rs"] {
            assert!(files.contains(&path.to_string()), "{path}: {delta:?}");
        }
        assert_equals_load(&context, &tree);

        let delta = save(&mut context, &tree, "src/lib.rs", "pub mod shared;\n");
        assert_eq!(key_of(&context, "src/shared.rs"), "tool::shared");
        assert_eq!(key_of(&context, "src/cli.rs"), "orphan:src/cli.rs");
        assert!(scoped(&delta).contains(&"src/cli.rs".to_string()), "{delta:?}");
        assert_equals_load(&context, &tree);
    }

    /// An explicit `[lib] path` is the lib's only root: creating that file
    /// adds the crate, and creating `src/lib.rs` afterwards changes nothing
    /// and reads no disk (its `mod child;` and the child file stay unread).
    #[test]
    fn an_explicit_lib_path_is_the_root_watched_and_src_lib_rs_is_not() {
        let tree = Tree::new("lib-path-created");
        tree.write("Cargo.toml", "[package]\nname = \"alpha\"\n\n[lib]\npath = \"src/custom.rs\"\n");
        tree.write("src/a.rs", "pub fn f() {}\n");
        let mut context = ProjectContext::load(&tree.0).unwrap();
        assert!(context.crates().is_empty());

        let delta = save(&mut context, &tree, "src/custom.rs", "pub mod a;\n");
        assert!(is_unknown(&delta), "{delta:?}");
        assert_eq!(crate_keys(&context), vec!["alpha"]);
        assert_eq!(key_of(&context, "src/a.rs"), "alpha::a");
        assert_equals_load(&context, &tree);

        let before = context.clone();
        tree.write("src/custom.rs", "pub mod a;\npub mod child;\n");
        tree.write("src/child.rs", "");
        let delta = save(&mut context, &tree, "src/lib.rs", "pub mod a;\n");
        assert_eq!(delta, None);
        assert_eq!(context, before, "the model is untouched");
    }

    /// The same for an explicit `[[bin]] path`: creating it adds the bin
    /// crate; `src/main.rs` is no root once `[[bin]]` entries exist, so
    /// creating it changes nothing and reads no disk.
    #[test]
    fn an_explicit_bin_path_is_the_root_watched_and_src_main_rs_is_not() {
        let tree = Tree::new("bin-path-created");
        tree.write(
            "Cargo.toml",
            "[package]\nname = \"alpha\"\n\n[[bin]]\nname = \"cli\"\npath = \"src/bin/cli.rs\"\n",
        );
        tree.write("src/lib.rs", "");
        tree.write("src/bin/run.rs", "pub fn run() {}\n");
        let mut context = ProjectContext::load(&tree.0).unwrap();
        assert_eq!(crate_keys(&context), vec!["alpha"]);

        let delta = save(&mut context, &tree, "src/bin/cli.rs", "mod run;\nfn main() {}\n");
        assert!(is_unknown(&delta), "{delta:?}");
        assert_eq!(crate_keys(&context), vec!["alpha", "cli"]);
        assert_eq!(key_of(&context, "src/bin/run.rs"), "cli::run");
        assert_equals_load(&context, &tree);

        let before = context.clone();
        tree.write("src/lib.rs", "pub mod child;\n");
        tree.write("src/child.rs", "");
        let delta = save(&mut context, &tree, "src/main.rs", "fn main() {}\n");
        assert_eq!(delta, None);
        assert_eq!(context, before, "the model is untouched");
    }

    /// A root present at load that lost the key collision is recorded as
    /// present: saving it is no creation, so it neither reloads nor reads
    /// the disk (which here holds a new `mod child;` in `lib.rs`).
    #[test]
    fn saving_a_root_that_lost_the_key_collision_does_not_reload() {
        let tree = Tree::new("losing-root-saved");
        tree.write("Cargo.toml", "[package]\nname = \"tool\"\n");
        tree.write("src/lib.rs", "");
        tree.write("src/main.rs", "fn main() {}\n");
        let mut context = ProjectContext::load(&tree.0).unwrap();
        assert_eq!(key_of(&context, "src/main.rs"), "orphan:src/main.rs");
        let before = context.clone();
        tree.write("src/lib.rs", "pub mod child;\n");
        tree.write("src/child.rs", "");

        let delta = context.source_changed(&RelPath::new("src/main.rs"), Some("fn main() {}\n// edited\n"));

        assert_eq!(delta, None);
        assert_eq!(context, before, "the model is untouched");
    }

    /// Note section 3.4: the cost of one crate re-scan on g-mesh's own
    /// `core` crate, against a cold `load` of the whole workspace. Run by
    /// hand (`--run-ignored only`); prints, asserts only that the probe
    /// re-scanned.
    #[test]
    #[ignore = "measurement"]
    fn measure_the_core_crate_re_scan() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let lib = RelPath::new("core/src/lib.rs");
        let text = std::fs::read_to_string(root.join(lib.as_str())).unwrap();
        let probe = format!("{text}\nmod gm507_probe_never_on_disk;\n");
        let started = std::time::Instant::now();
        let mut context = ProjectContext::load(&root).unwrap();
        let load = started.elapsed();
        let files = context.files.len();
        // The re-scan reads the disk, which never holds the probe, so every
        // probe save differs from the stored signature and re-scans.
        let mut rescans = Vec::new();
        for _ in 0..10 {
            let started = std::time::Instant::now();
            context.source_changed(&lib, Some(&probe));
            rescans.push(started.elapsed());
        }
        rescans.sort();
        let started = std::time::Instant::now();
        assert_eq!(context.source_changed(&lib, Some(&text)), None);
        let unchanged = started.elapsed();
        assert!(
            context.trees.iter().any(|tree| tree.signatures.contains_key(&lib)),
            "core/src/lib.rs is modeled"
        );
        println!(
            "GM507-MEASURE core re-scan: load {:?} ({files} files, {} crates); re-scan min {:?} median {:?} max {:?}; unchanged save {:?}",
            load,
            context.crates.len(),
            rescans[0],
            rescans[rescans.len() / 2],
            rescans[rescans.len() - 1],
            unchanged
        );
    }
}
