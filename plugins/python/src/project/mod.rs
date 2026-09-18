//! `ProjectContext`: the Python plugin's [`Extractor::Project`](g_mesh_plugin_sdk::Extractor::Project) -
//! which container key owns each `.py`/`.pyi` file, computed without
//! importing anything.
//!
//! # Scope: this is GM-295, not GM-296
//!
//! This module answers "what is the package tree" - a question about the
//! project as a whole, asked once per [`Extractor::load_project`] and again
//! on every `workspaceChanged`. It does not answer "what does this file
//! declare" - that is per-file, asked on every `extract`, and it is
//! tree-sitter-python's job (GM-296), not this one's. The seam between the
//! two is [`ProjectContext::container_for`], exactly the seam
//! `plugins/rust/src/project`'s own module doc names for GM-285/GM-286, and
//! GM-296's extractor is expected to call it once per file it is handed, the
//! same way `RustExtractor::extract` does.
//!
//! # Why no source is parsed here
//!
//! Rust needs `module_tree`'s own tree-sitter-shaped scanner because a
//! crate's module structure is a *statement* (`mod foo;`, `#[path = "…"]`)
//! that only source text carries. Python's is not: a module is a `.py` file,
//! a package is a directory holding one, and which package a module belongs
//! to is a pure function of its path on disk. So this module needs no
//! per-file text scan at all - [`g_mesh_plugin_sdk::walk_project`] (the same
//! walk `--bulk-index` itself uses, `.gitignore` and all) is both how the
//! files are found and, after one pass over the resulting paths, the whole
//! of what "what does this project's package tree look like" needs.
//!
//! # Decision 1: a module is a container in its own right, *and* a member of
//! its package
//!
//! `pkg/sub/mod.py` could be modeled either way the task poses it: `pkg.sub`
//! as the container with a `mod` node inside, or `pkg.sub.mod` as its own
//! container whose functions are members. Python needs **both answers at
//! once**, because it has two different ways to address the same module:
//!
//! - `from pkg.sub.mod import f` addresses `f` as a member of container
//!   `pkg.sub.mod` - the module is the container.
//! - `from pkg.sub import mod` addresses `mod` itself as a member of
//!   container `pkg.sub` - the module is a member of its *parent*.
//!
//! Rust resolves the equivalent question by making every `mod child;`
//! statement a member of the module that declares it (its own module doc,
//! and `graph::containers::parent_chain`'s own doc, both name this
//! explicitly as what keeps the parent chain gap-free). Python has no
//! `mod child;` statement to hang that membership on - directory structure
//! is the only signal - so this plugin's container model has to state the
//! same fact by construction instead: [`ContainerInfo::Module`] carries
//! *both* the module's own key (`pkg.sub.mod`, what its own top-level
//! declarations belong to) *and* its bare `name` (`mod`) plus its `parent`
//! (`pkg.sub`, what it is a member *of*). **GM-296's obligation**, recorded
//! here because this is the module that computes the two halves and GM-296
//! is who has to honor both: for every non-stub module file, in addition to
//! whatever `container = <own key>` declarations the file's own top-level
//! statements produce, emit exactly one additional node with
//! `container = <parent>`, `name = <bare name>`, `qualifiedName = <own
//! key>` - the same role a `WireNode::container`/`container_parent` pair
//! plays for an ordinary declaration, just carried by a node that represents
//! the *module itself* rather than one of its statements. This is not
//! optional decoration: without it, `from pkg.sub import mod` has nothing in
//! container `pkg.sub` named `mod` for the linker's `name`-key candidate
//! search to find.
//!
//! # Decision 2: `__init__.py`'s declarations are members of the package
//!
//! Python's own semantics make this a fact, not a choice: `f` defined in
//! `pkg/sub/__init__.py` is addressed as `pkg.sub.f`, not
//! `pkg.sub.__init__.f`. So [`ContainerInfo::Package`] gives `__init__.py`/
//! `__init__.pyi` the **same key its enclosing directory has** - there is no
//! separate `pkg.sub.__init__` container at all, unlike a Rust `mod.rs`
//! (which *is* its own module, `alpha::nested`, exactly as `nested.rs` would
//! be). A package's `parent`/`name` describe the *package itself* relative
//! to its own parent (`pkg.sub`'s parent is `pkg`, name `sub`) - the same
//! "announce yourself to your parent" shape [`ContainerInfo::Module`] carries
//! for an ordinary module, because a package is exactly the thing a `from
//! pkg import sub` addresses as a member of `pkg`.
//!
//! # Decision 3: namespace packages, and how the parent chain stays gap-free
//!
//! A PEP 420 namespace package (a directory with modules but no
//! `__init__.py`) still needs `core::graph::containers::parent_chain` to
//! have no gap at it, or `pkg.sub`-visible (`container(pkg.sub)`) code in a
//! deeper module could never resolve up through it. `parent_chain`'s own doc
//! (`core/src/graph/containers.rs`) is explicit about the mechanism this
//! plugin has to use: a container only has a `parentKey` row once it has at
//! least one member, so the fix is **the same one Decision 1 already
//! builds**, not a second one. Every module and every `__init__.py` file
//! announces itself as a *member of its immediate parent* (Decisions 1/2's
//! extra node); a namespace package that holds at least one direct module or
//! `__init__.py`-backed subpackage therefore always has a member the moment
//! anything is emitted for it, whether or not the namespace package itself
//! has a file of its own. Concretely: `pkg/sub/mod.py` with no
//! `pkg/sub/__init__.py` still makes `pkg.sub` exist as a container, because
//! `mod`'s own announcement node carries `container = "pkg.sub"` - the
//! namespace package needs no announcement of its own for *this* to work.
//!
//! What this does **not** close, and is accepted rather than hidden: an
//! *intermediate* namespace package with no direct module or subpackage of
//! its own - only deeper namespace packages beneath it (`pkg/sub/subsub/mod.py`
//! with neither `pkg/sub/` nor `pkg/sub/subsub/` holding an `__init__.py`,
//! and nothing directly inside `pkg/sub/` itself) - has no file anywhere
//! that is *its own* announcer, because nothing on disk is uniquely "the
//! file that declares `pkg.sub` is a member of `pkg`" the way a Rust `mod
//! sub;` statement unambiguously is. Manufacturing one deterministically
//! (say, the lexicographically first file anywhere beneath it) was
//! considered and rejected: it would make one file, chosen for a reason
//! invisible in its own text, respond to `workspaceChanged` by starting or
//! stopping being responsible for a container-membership node that has
//! nothing to do with what it declares - a form of non-local coupling this
//! plugin's own "no source is parsed" simplicity (this module's own opening
//! section) exists specifically to avoid. The gap this leaves is the safe
//! kind `parent_chain`'s own doc describes: "a gap can make the visibility
//! check refuse a link that a complete chain would allow (a missing edge),
//! never allow one it should refuse (a wrong edge).
//!
//! GM-313 measured how often the shape occurs, because this doc originally
//! called it "an unusual repository shape" and that was a guess. It is not
//! unusual: django/django has **9** directories with no `__init__` and no
//! direct `.py` but Python beneath them, and pallets/flask has **6**. What is
//! true, and is the reason the gap stays accepted, is *where* they are - in
//! both corpora every one of them is a test fixture tree or an examples
//! directory (`tests/test_apps`, `examples/tutorial`,
//! `tests/migrations/faulty_migrations/namespace`), never the importable
//! package tree the index is asked about. So the correct claim is not that
//! the shape is rare but that it is rare *where it would cost anything*, and
//! the measurement says so rather than intuition.
//!
//! Confirmed permanent-by-design (GM-313). GM-296 was not expected to solve
//! it and neither is anything else until a real project is hurt by it; a
//! future `DECLARATION_OF`-style mechanism or an explicit "announce every
//! namespace-package ancestor, deduplicated by core" wire addition would be
//! the place to.
//!
//! # Decision 4: roots
//!
//! A **root** is a directory this plugin treats as a Python import root - a
//! `sys.path` entry, in effect - so that a file's dotted key is computed
//! relative to it. Three sources, exactly the task's own list:
//!
//! 1. **`pyproject.toml` hints** ([`pyproject::root_hints`]): `[tool.poetry]
//!    packages[].from` and `[tool.setuptools] package-dir[""]`. Trusted
//!    unconditionally - a project that declares its own layout is not
//!    second-guessed by what this plugin happens to find on disk.
//! 2. **`src/` layout**: a directory literally named `src` at the project
//!    root counts as a root *if* the walk actually found a `.py`/`.pyi` file
//!    under it - an empty or absent `src/` contributes nothing, so this
//!    plugin does not invent a root from a directory that exists for some
//!    other reason (a C extension's own `src/`, say, in a mixed-language
//!    repository).
//! 3. **The project root itself, as a fallback** - used *only* when neither
//!    of the above named anything. This is what models "a flat directory of
//!    scripts with no package at all": with no `pyproject.toml` hint and no
//!    `src/`, every `.py` file the walk finds is a top-level module or
//!    package rooted at `""`, `parent: None` - exactly as reachable as a
//!    real package, because it genuinely is one (Python does not
//!    distinguish "a package" from "a directory of scripts" at the
//!    filesystem level; the distinction PEP 420 draws is `__init__.py` or
//!    not, not "intentional package" or not).
//!
//! **Why the fallback is conditional, not unconditional.** If `""` were
//! *always* a root, in addition to `src/` or a `pyproject.toml` hint, then a
//! project that has explicitly said "my code lives under `src/`" would still
//! have every stray top-level `.py` file (a `conftest.py`, a `noxfile.py`, a
//! one-off `tools/generate.py`) swept into a top-level package namespace it
//! never claimed to have. Once a project names a specific root, everything
//! outside every named root falls to **orphan** (Decision 5) instead - which
//! is also what gives "a module with no root" (the task's own acceptance
//! case) a concrete, realistic shape: a `src/`-layout project with one file
//! sitting beside `src/` rather than inside it.
//!
//! **Several independent roots.** One project can have more than one root,
//! but only from *one* of the three sources - they are tried in order and
//! the first that names anything wins outright, which is point 1's "not
//! second-guessed by what this plugin happens to find on disk" applied to
//! the whole list rather than to a single hint. So a project declaring two
//! `[tool.poetry] packages` entries with different `from`s gets both of
//! them as roots, while a project declaring one hint *and* holding an
//! unrelated `src/` directory gets the hint alone - that `src/`'s files
//! become orphans (Decision 5), the missing-answer side of the trade, not a
//! phantom root the project never claimed. Where there is more than one,
//! each is modeled as its own [`Root`] and
//! [`ProjectContext::roots`] returns them all - the same "more than one,
//! each independent" shape
//! `plugins/rust/src/project::Crate` uses for several crates in one
//! workspace, chosen over trying to nest or merge them because nothing about
//! Python's import system requires two `sys.path` entries to relate to each
//! other at all.
//!
//! A file under more than one root's directory (an unusual, arguably
//! misconfigured case - two roots, one nested inside the other) is assigned
//! to the **longest** matching root, so the more specific one wins; see
//! [`owning_root`].
//!
//! # Decision 5: the orphan container's key
//!
//! A `.py`/`.pyi` file the walk finds but no root's directory contains (see
//! Decision 4) is still indexed - the task's own words, "still get indexed
//! under a container derived from their path, marked so a reader can tell" -
//! as [`ContainerInfo::Orphan`], key `"orphan:<path>"`. Exactly
//! `plugins/rust/src/project`'s own Decision 5, and the same collision-proof
//! argument applies verbatim: a real Python dotted key is one or more `.`-
//! joined identifiers, which by definition contains neither `/` nor a lone
//! `:`, and a project-relative path always contains at least one `/` (a
//! bare top-level filename still carries a `.py`/`.pyi` extension, i.e. a
//! `.`, which a real key's *segments* never do - Python identifiers cannot
//! contain `.`). The `orphan:` prefix is redundant with that guarantee on
//! its own but keeps the convention legible without already knowing it -
//! the same reasoning `plugins/rust/src/project`'s own doc gives.
//!
//! # Decision 6: `.pyi` stubs
//!
//! 3.0.0 deliberately did not build the multi-file symbol model
//! (`DECLARATION_OF`) - see `docs/architecture/multi-language-plugins.md`'s
//! "Symbols declared in several files: designed, not built", which lists
//! `.pyi` stubs by name as exactly this case. Two options were on the table,
//! per the task's own text: index a stub as an ordinary module in its own
//! container, or skip it. **Chosen: index the file (it is never silently
//! dropped - the plugin claims `.pyi` in its manifest, and a file it claims
//! but never reports would fail `g-mesh plugins check`'s own shape
//! expectations), but never let it contribute a container member or a
//! Decision 1/2 self-announcement.**
//!
//! The reason is the same "cannot produce a wrong answer" the task asks for.
//! A `.pyi` stub beside its `.py` module would, under the "ordinary module"
//! option, compute the *exact same* dotted key as its sibling (`pkg/mod.pyi`
//! and `pkg/mod.py` both key to `pkg.mod`) - which is correct for a
//! `DECLARATION_OF`-aware future, but wrong *today*: two different files
//! would each try to emit the Decision 1 self-announcement node
//! (`container = pkg`, `name = "mod"`), and the linker's own contract
//! refuses an ambiguous `name`-keyed candidate rather than guessing - so
//! `from pkg import mod` would silently stop resolving the moment a stub
//! appeared, which is a regression a plugin update must never cause. Worse,
//! a stub whose signatures disagree with its module (the whole reason stubs
//! exist - a C extension's `.pyi`, or a hand-maintained stub for
//! generated code) would offer *duplicate, possibly conflicting*
//! declarations under one container with no way for a reader to tell which
//! one is real. Skipping declarations entirely for a stub produces a
//! strictly *missing* answer (the stub's own types are simply not indexed
//! yet), never a wrong one - the same "missing edge beats wrong edge" rule
//! this whole design doc applies everywhere else.
//!
//! [`ContainerInfo::Stub`] still carries the **same key its sibling module
//! would compute** (not a mangled or `orphan:`-prefixed one), specifically
//! so that when `DECLARATION_OF` lands, GM-296 (or whichever task adds it)
//! can link a stub's declarations to their real module by a
//! `qualifiedName`-keyed placeholder in that shared container with no
//! re-derivation - see `docs/architecture/multi-language-plugins.md`'s own
//! `DECLARATION_OF` sketch (`declaration node -> defining node`, linked by a
//! container-scoped `qualifiedName` placeholder), which this key is chosen
//! to be ready for.
//!
//! # What GM-296 inherits
//!
//! The public surface below mirrors `plugins/rust/src/project::ProjectContext`
//! field for field: [`ProjectContext::load`], [`ProjectContext::roots`]
//! (Rust's `crates()` - renamed because a Python root is a `sys.path` entry
//! that may hold many top-level packages, not one compiled unit, so
//! "packages" would overstate what one `Root` is), [`ProjectContext::container_for`]
//! (the exact seam GM-296 calls once per file, exactly as
//! `RustExtractor::extract` calls its Rust counterpart), and
//! [`ProjectContext::notes`].
//!
//! # Decision 8 (GM-296): "is this dotted name one of ours?"
//!
//! One question GM-296 turned out to need that this task did not anticipate,
//! added here rather than in the extractor because it is a fact about the
//! *project*, not about any one file: [`ProjectContext::has_container`].
//!
//! Rust can tell `use serde::Serialize;` from `use crate::a::b::C;` because
//! `Cargo.toml` names every crate the workspace holds, so
//! `plugins/rust/src/extractor` emits an `external_module` node for the first
//! and a `resolved_module` placeholder for the second. Python has no
//! equivalent manifest - `pyproject.toml`'s `[project] dependencies` is a
//! list of *distribution* names, which are frequently not the import names
//! (`pip install pillow` imports as `PIL`), and reading it would be reading
//! the wrong list. The one honest source is the package tree this module
//! already computed: `import pkg.sub` names something of ours exactly when
//! `pkg.sub` is a container key, or a prefix of one, among the files the walk
//! found.
//!
//! The failure direction is the safe one. A container this model never saw -
//! a module created since the last `load`, a file the walk could not read -
//! is answered `false`, so the extractor emits an `external_module` node and
//! the import simply does not link: a missing edge, never a wrong one. The
//! opposite mistake is impossible, because the set is built from files that
//! really exist.

mod pyproject;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use g_mesh_plugin_sdk::{walk_project, RelPath};

/// Directory names this plugin's own project-model walk never descends
/// into, beyond [`g_mesh_plugin_sdk::BASELINE_EXCLUDED_DIRS`] - virtual
/// environments and their caches, never source in any Python project.
///
/// `node_modules` joined the list in GM-299 and is the one entry that is not
/// a Python artefact. A Python project may perfectly well have one - a web
/// application with a JavaScript front end - and it is never that project's
/// own Python source. What forced the question is that `crate::semantic`'s
/// second resolution branch *invites* one: a user told to run `npm install
/// pyright` in their project gets a `node_modules` holding pyright's bundled
/// typeshed, which is **5,205 `.pyi` files** (measured, 1.1.414). Without
/// this entry every one of them would be indexed as a declaration of the
/// project. The same reasoning as `site-packages`, arriving through a
/// different package manager.
///
/// Duplicated as a plain constant here, and again in `plugin.toml`'s
/// `[plugin.workspace] exclude_dirs`, rather than shared as code: this
/// module's own walk (root detection, run once per [`ProjectContext::load`])
/// and the SDK's per-`extract` walk are two different call sites with no
/// common caller to thread a slice through, and `plugins/sdk`'s own
/// `manifest` module doc names this exact drift as accepted precedent
/// (`ignorePolicy.ts`'s `HARD_EXCLUDED_DIRS` vs. `plugin.toml`, "the same
/// list written twice, with a comment explaining how they relate").
pub(crate) const EXCLUDE_DIRS: [&str; 7] =
    [".venv", "venv", "__pycache__", ".tox", ".mypy_cache", "site-packages", "node_modules"];

const PY_EXTENSION: &str = ".py";
const PYI_EXTENSION: &str = ".pyi";
const INIT_STEM: &str = "__init__";

/// One `sys.path`-equivalent root this project model found - see this
/// module's doc, Decision 4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    /// This root's own directory, project-relative. `""` for the project
    /// root itself.
    pub dir: RelPath,
}

/// What [`ProjectContext::container_for`] answers about one file - see this
/// module's doc, Decisions 1, 2 and 6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerInfo {
    /// An ordinary module file (any `.py` file except `__init__.py`).
    Module {
        /// This module's own container key, `pkg.sub.mod` - what its own
        /// top-level declarations belong to (Decision 1).
        key: String,
        /// The enclosing package's key. `None` only when this module sits
        /// directly in its root with no package above it.
        parent: Option<String>,
        /// The bare module name (`mod`, not `mod.py`) - what a lookup
        /// addresses this module by as a member of `parent`
        /// (`from pkg.sub import mod`). See this module's doc, Decision 1,
        /// for the node GM-296 must emit to back this.
        name: String,
    },
    /// `__init__.py`: the package's own file. Its container key *is* the
    /// package's key (Decision 2) - there is no separate `pkg.sub.__init__`
    /// container.
    Package {
        /// The package's own key, `pkg.sub`.
        key: String,
        /// The enclosing package's key. `None` only for a top-level package.
        parent: Option<String>,
        /// The bare package name (`sub`) - what a lookup addresses this
        /// package by as a member of `parent` (`from pkg import sub`).
        name: String,
    },
    /// A `.pyi` stub - see this module's doc, Decision 6. `key` is the same
    /// dotted path its sibling module (real or hypothetical) would compute,
    /// kept for a future `DECLARATION_OF` link, but this file must never be
    /// treated as a container member: no declaration or Decision 1/2
    /// self-announcement should be emitted for it.
    Stub { key: String },
    /// A `.py`/`.pyi` file no root reaches (Decision 4/5). `key` can never
    /// collide with a real dotted path - see this module's doc.
    Orphan { key: String },
}

/// The Python plugin's whole project model: every root this project's
/// layout and `pyproject.toml` declare, and which container owns each file
/// reachable from one of them. See this module's doc for what is and is not
/// modeled, and its "What GM-296 inherits" section for how the extractor is
/// expected to consume it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectContext {
    roots: Vec<Root>,
    files: BTreeMap<RelPath, ContainerInfo>,
    /// Every dotted key this project's files make addressable, each file's
    /// own key together with every ancestor package of it - see this module's
    /// doc, Decision 8, and [`ProjectContext::has_container`].
    ///
    /// Ancestors are materialized rather than computed by prefix-matching at
    /// lookup time because a prefix match is wrong in one direction that
    /// matters: `pkg.subtle` starts with `pkg.sub` as a *string* and is not
    /// under it as a *package*. Storing `pkg` and `pkg.sub` as their own
    /// entries makes the question an exact set membership, which cannot get
    /// that case wrong.
    containers: BTreeSet<String>,
    /// Everything this load could not honestly resolve - a `pyproject.toml`
    /// that would not parse. Never fatal, the same "a project model with
    /// notes is still a complete, deterministic answer for everything it
    /// *could* resolve" rule `plugins/rust/src/project::ProjectContext`
    /// documents for itself.
    notes: Vec<String>,
}

impl ProjectContext {
    /// Builds the project model for the project at `root` (an absolute
    /// path, as [`Extractor::load_project`](g_mesh_plugin_sdk::Extractor::load_project)
    /// hands it). Called once by `--bulk-index`, and again on every
    /// `workspaceChanged` - see `crate::main`'s module doc and the SDK's own
    /// `run::Session::load_project`, which is what actually calls this again
    /// after a `pyproject.toml` edit; nothing in this module watches the
    /// filesystem itself.
    ///
    /// Never fails. A project with no Python files at all, or no
    /// `pyproject.toml`, still yields a complete (if minimal) model - see
    /// this module's doc, Decision 4, for the fallback root that makes that
    /// true.
    pub fn load(root: &Path) -> anyhow::Result<Self> {
        let mut notes = Vec::new();
        let extensions = vec![PY_EXTENSION.to_string(), PYI_EXTENSION.to_string()];
        let exclude: Vec<String> = EXCLUDE_DIRS.iter().map(|dir| (*dir).to_string()).collect();
        let walked = walk_project(root, &extensions, &exclude);

        let mut root_dirs: Vec<RelPath> = Vec::new();
        for hint in pyproject::root_hints(root, &mut notes) {
            push_unique(&mut root_dirs, hint);
        }
        if root_dirs.is_empty() && has_src_layout(&walked) {
            push_unique(&mut root_dirs, RelPath::new("src"));
        }
        if root_dirs.is_empty() {
            root_dirs.push(RelPath::new(""));
        }
        // Deepest (most specific) first, so a file under more than one
        // root's directory resolves to the more specific one - see this
        // module's doc, Decision 4.
        root_dirs.sort_by(|a, b| depth(b).cmp(&depth(a)).then_with(|| a.as_str().cmp(b.as_str())));

        let mut files = BTreeMap::new();
        let mut containers = BTreeSet::new();
        for path in &walked {
            let info = match owning_root(&root_dirs, path) {
                Some(root_dir) => container_info_for(path, root_dir, &mut notes),
                None => ContainerInfo::Orphan { key: orphan_key(path) },
            };
            register_container(&mut containers, &info);
            files.insert(path.clone(), info);
        }

        Ok(Self { roots: root_dirs.into_iter().map(|dir| Root { dir }).collect(), files, containers, notes })
    }

    /// Whether `key` names a module or package **of this project** - the
    /// question an absolute `import a.b` has to answer before the extractor
    /// can decide between a `resolved_module` placeholder (ours, so core may
    /// link it onto a container node) and an `external_module` node (a
    /// third-party distribution or the standard library, which core never
    /// links).
    ///
    /// Answered from the package tree [`load`](Self::load) already walked -
    /// see this module's doc, Decision 8, for why that is the only honest
    /// source and why a `false` answer can only ever cost a missing edge.
    /// A `.pyi` stub contributes its key here even though it contributes no
    /// declarations (Decision 6): the stub's existence is still evidence that
    /// the dotted name is this project's own, and an import of it that
    /// resolves onto an empty container is a truthful "we have this module
    /// and it declares nothing we indexed".
    pub fn has_container(&self, key: &str) -> bool {
        self.containers.contains(key)
    }

    /// Every root this project model found, in the order [`load`](Self::load)
    /// resolved them (deepest/most specific first - see this module's doc,
    /// Decision 4). Named `roots`, not `packages` (compare
    /// `plugins/rust/src/project::ProjectContext::crates`) because one root
    /// is a `sys.path` entry that may hold many top-level packages and
    /// modules, not a single compiled unit the way a Rust crate is.
    pub fn roots(&self) -> &[Root] {
        &self.roots
    }

    /// The container `path` belongs to - see [`ContainerInfo`]. Always
    /// answers something: every `.py`/`.pyi` file is a module, a package's
    /// own file, a stub, or an orphan, never neither. A `path` this model
    /// never saw during [`load`](Self::load) (the extension is claimed but
    /// the file did not exist, or was created since) is answered exactly as
    /// if it had been walked - the same "always answers" guarantee
    /// `plugins/rust/src/project::ProjectContext::container_for` documents
    /// for itself, and for the same reason: `fileChanged` calls this too.
    ///
    /// A path under [`EXCLUDE_DIRS`] (or the SDK's own
    /// [`g_mesh_plugin_sdk::BASELINE_EXCLUDED_DIRS`]) is answered as an
    /// orphan without consulting the roots at all - `load`'s own walk would
    /// never have reached it either, and core's watcher is not expected to
    /// route such a path here (`plugin.toml`'s `exclude_dirs` is read for
    /// exactly that), but "answers as if it had been walked" has to mean
    /// *the same* walk, exclusions included, or the two could disagree about
    /// a single file depending on whether it happened to exist yet.
    pub fn container_for(&self, path: &RelPath) -> ContainerInfo {
        if let Some(info) = self.files.get(path) {
            return info.clone();
        }
        if path_is_excluded(path) {
            return ContainerInfo::Orphan { key: orphan_key(path) };
        }
        let mut discard = Vec::new();
        let mut root_dirs: Vec<RelPath> = self.roots.iter().map(|root| root.dir.clone()).collect();
        root_dirs.sort_by(|a, b| depth(b).cmp(&depth(a)).then_with(|| a.as_str().cmp(b.as_str())));
        match owning_root(&root_dirs, path) {
            Some(root_dir) => container_info_for(path, root_dir, &mut discard),
            None => ContainerInfo::Orphan { key: orphan_key(path) },
        }
    }

    /// Everything this load could not honestly resolve - see this struct's
    /// own field doc.
    pub fn notes(&self) -> &[String] {
        &self.notes
    }
}

/// Pushes `dir` onto `root_dirs` unless an equal directory is already
/// present - `pyproject.toml` hints and the `src/` heuristic can name the
/// same directory (a `package-dir = {"" = "src"}` hint alongside a real
/// `src/` layout), and that must not produce two [`Root`]s for one
/// directory.
fn push_unique(root_dirs: &mut Vec<RelPath>, dir: RelPath) {
    if !root_dirs.contains(&dir) {
        root_dirs.push(dir);
    }
}

/// Whether the walk found at least one `.py`/`.pyi` file under a top-level
/// `src/` directory - see this module's doc, Decision 4, for why an empty or
/// absent `src/` must not become a root.
fn has_src_layout(walked: &[RelPath]) -> bool {
    walked.iter().any(|path| path.as_str().starts_with("src/"))
}

/// Whether any directory segment of `path` is one [`walk_project`] would
/// never descend into - [`EXCLUDE_DIRS`] or the SDK's own
/// [`g_mesh_plugin_sdk::BASELINE_EXCLUDED_DIRS`]. Only [`ProjectContext::container_for`]'s
/// fallback for a path [`ProjectContext::load`] never walked needs this -
/// every path the walk itself produced already satisfies it by construction.
fn path_is_excluded(path: &RelPath) -> bool {
    let (dirs, _) = path.as_str().rsplit_once('/').unwrap_or(("", path.as_str()));
    dirs.split('/').any(|segment| {
        EXCLUDE_DIRS.contains(&segment) || g_mesh_plugin_sdk::BASELINE_EXCLUDED_DIRS.contains(&segment)
    })
}

/// The number of path segments in `dir` - `0` for `""` (the project root),
/// `1` for `"src"`, `2` for `"src/pkg"`. Used only to sort roots deepest
/// first; see [`ProjectContext::load`].
fn depth(dir: &RelPath) -> usize {
    if dir.as_str().is_empty() {
        0
    } else {
        dir.as_str().matches('/').count() + 1
    }
}

/// The most specific root in `root_dirs` (already sorted deepest first)
/// whose directory contains `path`, or `None` if none does - see this
/// module's doc, Decision 4/5.
fn owning_root<'a>(root_dirs: &'a [RelPath], path: &RelPath) -> Option<&'a RelPath> {
    root_dirs.iter().find(|dir| is_under(dir, path))
}

/// Whether `path` sits at or under directory `dir` - `dir == ""` matches
/// every path (the project root contains everything under it by
/// definition).
fn is_under(dir: &RelPath, path: &RelPath) -> bool {
    if dir.as_str().is_empty() {
        return true;
    }
    path.as_str().strip_prefix(dir.as_str()).is_some_and(|rest| rest.starts_with('/'))
}

/// Computes [`ContainerInfo`] for `path`, known to sit under `root_dir` -
/// see this module's doc, Decisions 1, 2 and 6.
fn container_info_for(path: &RelPath, root_dir: &RelPath, notes: &mut Vec<String>) -> ContainerInfo {
    let relative = if root_dir.as_str().is_empty() {
        path.as_str()
    } else {
        // `is_under` (the only caller path that reaches here through
        // `owning_root`) already proved this prefix, and `container_for`'s
        // own fallback path re-derives the same `root_dir` the same way -
        // so this can only fail on a caller bug, never on project content.
        path.as_str()
            .strip_prefix(root_dir.as_str())
            .and_then(|rest| rest.strip_prefix('/'))
            .unwrap_or(path.as_str())
    };

    let is_stub = path.extension().as_deref() == Some(PYI_EXTENSION);
    let mut segments: Vec<&str> = relative.split('/').collect();
    // `unwrap`: `relative` is never empty - it is a walked file's own path,
    // which always has at least one segment (the file name itself).
    let file_name = segments.pop().unwrap();
    let stem = file_name
        .strip_suffix(PY_EXTENSION)
        .or_else(|| file_name.strip_suffix(PYI_EXTENSION))
        .unwrap_or(file_name);

    if stem == INIT_STEM {
        if segments.is_empty() {
            // `__init__.py` directly inside a root has no package directory
            // of its own to be the key of - a degenerate layout (the root
            // itself would be the "package", which has no dotted name).
            // Noted rather than silently mis-keyed; see this module's doc,
            // Decision 4's fallback-root discussion for the same
            // "acceptable, documented gap" standard.
            notes.push(format!(
                "{path}: __init__ directly under root {root_dir:?} has no package directory to name - not \
                 indexed as a package"
            ));
            return ContainerInfo::Orphan { key: orphan_key(path) };
        }
        let key = segments.join(".");
        let name = (*segments.last().expect("checked non-empty")).to_string();
        let parent = (segments.len() > 1).then(|| segments[..segments.len() - 1].join("."));
        let package = ContainerInfo::Package { key, parent, name };
        return if is_stub { Stubbed::key_of(package) } else { package };
    }

    let name = stem.to_string();
    let parent = (!segments.is_empty()).then(|| segments.join("."));
    let mut key_segments = segments;
    key_segments.push(stem);
    let key = key_segments.join(".");
    let module = ContainerInfo::Module { key, parent, name };
    if is_stub {
        Stubbed::key_of(module)
    } else {
        module
    }
}

/// Turns a computed [`ContainerInfo::Module`]/[`ContainerInfo::Package`]
/// into [`ContainerInfo::Stub`], keeping only its key - see this module's
/// doc, Decision 6, for why a stub carries a key but never a `parent`/`name`
/// self-announcement.
struct Stubbed;
impl Stubbed {
    fn key_of(info: ContainerInfo) -> ContainerInfo {
        match info {
            ContainerInfo::Module { key, .. } | ContainerInfo::Package { key, .. } => {
                ContainerInfo::Stub { key }
            }
            other => other,
        }
    }
}

/// The synthetic container key for an unreachable file - see this module's
/// doc, Decision 5, for why this exact shape cannot collide with a real
/// dotted key.
fn orphan_key(path: &RelPath) -> String {
    format!("orphan:{path}")
}

/// Records `info`'s dotted key, and every package above it, in the set
/// [`ProjectContext::has_container`] answers from - see this module's doc,
/// Decision 8.
///
/// An [`ContainerInfo::Orphan`] contributes nothing: its key is
/// `orphan:<path>`, which by construction is not a dotted name any `import`
/// statement can spell, so recording it could only ever make a nonsense
/// import look like one of ours.
fn register_container(containers: &mut BTreeSet<String>, info: &ContainerInfo) {
    let key = match info {
        ContainerInfo::Module { key, .. }
        | ContainerInfo::Package { key, .. }
        | ContainerInfo::Stub { key } => key.as_str(),
        ContainerInfo::Orphan { .. } => return,
    };
    let mut prefix = String::new();
    for segment in key.split('.') {
        if !prefix.is_empty() {
            prefix.push('.');
        }
        prefix.push_str(segment);
        containers.insert(prefix.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree(std::path::PathBuf);

    impl Tree {
        /// Unique per call, not just per `name` - `cargo test` runs these
        /// `#[test]` functions concurrently in one process, and two tests
        /// racing on the same path is exactly the flake
        /// `plugins/rust/src/project`'s own `Tree::new` doc comment already
        /// found once.
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            let root = std::env::temp_dir()
                .join(format!("g-mesh-plugin-python-project-{}-{name}-{nanos}", std::process::id()));
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
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn module(key: &str, parent: Option<&str>, name: &str) -> ContainerInfo {
        ContainerInfo::Module {
            key: key.to_string(),
            parent: parent.map(str::to_string),
            name: name.to_string(),
        }
    }

    fn package(key: &str, parent: Option<&str>, name: &str) -> ContainerInfo {
        ContainerInfo::Package {
            key: key.to_string(),
            parent: parent.map(str::to_string),
            name: name.to_string(),
        }
    }

    // --- flat package -------------------------------------------------------

    #[test]
    fn a_flat_top_level_package_has_no_root_hint_and_no_src_dir() {
        let tree = Tree::new("flat-package");
        tree.write("pkg/__init__.py", "");
        tree.write("pkg/mod.py", "");
        let context = ProjectContext::load(&tree.0).unwrap();
        assert_eq!(context.roots(), &[Root { dir: RelPath::new("") }]);
        assert_eq!(context.container_for(&RelPath::new("pkg/__init__.py")), package("pkg", None, "pkg"));
        assert_eq!(context.container_for(&RelPath::new("pkg/mod.py")), module("pkg.mod", Some("pkg"), "mod"));
    }

    /// Core's `parent_chain` acceptance case: `pkg.sub.mod`'s parent must be
    /// exactly `pkg.sub`, so a `container(pkg.sub)`-visible symbol resolves.
    #[test]
    fn container_keys_and_parents_match_what_core_parent_chain_expects() {
        let tree = Tree::new("chain");
        tree.write("pkg/__init__.py", "");
        tree.write("pkg/sub/__init__.py", "");
        tree.write("pkg/sub/mod.py", "");
        let context = ProjectContext::load(&tree.0).unwrap();
        assert_eq!(context.container_for(&RelPath::new("pkg/__init__.py")), package("pkg", None, "pkg"));
        assert_eq!(
            context.container_for(&RelPath::new("pkg/sub/__init__.py")),
            package("pkg.sub", Some("pkg"), "sub")
        );
        assert_eq!(
            context.container_for(&RelPath::new("pkg/sub/mod.py")),
            module("pkg.sub.mod", Some("pkg.sub"), "mod")
        );
    }

    // --- src layout -----------------------------------------------------------

    #[test]
    fn a_src_layout_strips_src_from_every_key() {
        let tree = Tree::new("src-layout");
        tree.write("src/pkg/__init__.py", "");
        tree.write("src/pkg/mod.py", "");
        let context = ProjectContext::load(&tree.0).unwrap();
        assert_eq!(context.roots(), &[Root { dir: RelPath::new("src") }]);
        assert_eq!(
            context.container_for(&RelPath::new("src/pkg/mod.py")),
            module("pkg.mod", Some("pkg"), "mod")
        );
    }

    /// An empty or absent `src/` must not become a root - see the module
    /// doc, Decision 4.
    #[test]
    fn an_src_directory_with_no_python_in_it_is_not_a_root() {
        let tree = Tree::new("src-no-python");
        tree.write("src/README.md", "");
        tree.write("pkg.py", "");
        let context = ProjectContext::load(&tree.0).unwrap();
        assert_eq!(context.roots(), &[Root { dir: RelPath::new("") }]);
        assert_eq!(context.container_for(&RelPath::new("pkg.py")), module("pkg", None, "pkg"));
    }

    // --- namespace package -----------------------------------------------------

    #[test]
    fn a_namespace_package_has_no_init_but_still_has_a_dotted_key() {
        let tree = Tree::new("namespace");
        tree.write("pkg/__init__.py", "");
        tree.write("pkg/plugins/foo.py", ""); // `pkg/plugins/` has no __init__.py
        let context = ProjectContext::load(&tree.0).unwrap();
        assert_eq!(
            context.container_for(&RelPath::new("pkg/plugins/foo.py")),
            module("pkg.plugins.foo", Some("pkg.plugins"), "foo")
        );
    }

    /// Decision 3's own claim: a namespace package that holds a direct
    /// module is never a parent-chain gap, because that module's own
    /// Decision-1 self-announcement (its `parent`, here) is exactly the
    /// membership `containers.parentKey` needs - this test is the
    /// project-model half of that guarantee (GM-296 emits the node; this
    /// module computes the `parent` it carries).
    #[test]
    fn a_namespace_packages_direct_module_carries_its_parent_for_the_gap_to_close_on() {
        let tree = Tree::new("namespace-gap");
        tree.write("pkg/plugins/foo.py", ""); // no __init__.py anywhere
        let context = ProjectContext::load(&tree.0).unwrap();
        let ContainerInfo::Module { parent, .. } = context.container_for(&RelPath::new("pkg/plugins/foo.py"))
        else {
            panic!("expected a Module");
        };
        assert_eq!(parent.as_deref(), Some("pkg.plugins"));
    }

    // --- a module with no root ---------------------------------------------

    /// Once `src/` is a root, a file sitting *beside* it (not under it) is
    /// reachable from no root - the concrete shape of "a module with no
    /// root" this module's doc, Decision 4, describes.
    #[test]
    fn a_file_outside_every_declared_root_is_an_orphan() {
        let tree = Tree::new("no-root");
        tree.write("src/pkg/mod.py", "");
        tree.write("tools/generate.py", "");
        let context = ProjectContext::load(&tree.0).unwrap();
        assert_eq!(
            context.container_for(&RelPath::new("tools/generate.py")),
            ContainerInfo::Orphan { key: "orphan:tools/generate.py".to_string() }
        );
        // ...and the orphan key can never collide with a real dotted path.
        assert!(!context.container_for(&RelPath::new("tools/generate.py")).eq(&module(
            "tools.generate",
            None,
            "generate"
        )));
    }

    // --- pyproject hint ------------------------------------------------------

    #[test]
    fn an_explicit_pyproject_packages_hint_overrides_the_default_root() {
        let tree = Tree::new("pyproject-hint");
        tree.write("pyproject.toml", "[tool.poetry]\npackages = [{ include = \"pkg\", from = \"lib\" }]\n");
        tree.write("lib/pkg/mod.py", "");
        let context = ProjectContext::load(&tree.0).unwrap();
        assert_eq!(context.roots(), &[Root { dir: RelPath::new("lib") }]);
        assert_eq!(
            context.container_for(&RelPath::new("lib/pkg/mod.py")),
            module("pkg.mod", Some("pkg"), "mod")
        );
    }

    #[test]
    fn several_independent_pyproject_roots_are_each_modeled() {
        let tree = Tree::new("several-roots");
        tree.write(
            "pyproject.toml",
            "[tool.poetry]\npackages = [{ include = \"a\", from = \"src\" }, { include = \"b\", from = \"vendor-src\" }]\n",
        );
        tree.write("src/a/mod.py", "");
        tree.write("vendor-src/b/mod.py", "");
        let context = ProjectContext::load(&tree.0).unwrap();
        let mut dirs: Vec<&str> = context.roots().iter().map(|root| root.dir.as_str()).collect();
        dirs.sort_unstable();
        assert_eq!(dirs, vec!["src", "vendor-src"]);
        assert_eq!(context.container_for(&RelPath::new("src/a/mod.py")), module("a.mod", Some("a"), "mod"));
        assert_eq!(
            context.container_for(&RelPath::new("vendor-src/b/mod.py")),
            module("b.mod", Some("b"), "mod")
        );
    }

    /// The other half of "the first source that names anything wins
    /// outright": a project that declares its own root keeps that root even
    /// when a `src/` directory holding Python sits right beside it. Pinned
    /// because the doc used to promise the opposite (the two sources adding
    /// up), and because the failure it prevents is silent - `src/side.py`
    /// keyed as a top-level module `side` would be a root the project never
    /// claimed, where an orphan is merely an answer withheld.
    #[test]
    fn a_declared_root_is_not_joined_by_a_stray_src_directory() {
        let tree = Tree::new("hint-beats-src");
        tree.write("pyproject.toml", "[tool.poetry]\npackages = [{ include = \"pkg\", from = \"lib\" }]\n");
        tree.write("lib/pkg/mod.py", "");
        tree.write("src/side.py", "");
        let context = ProjectContext::load(&tree.0).unwrap();
        assert_eq!(context.roots(), &[Root { dir: RelPath::new("lib") }]);
        assert_eq!(
            context.container_for(&RelPath::new("src/side.py")),
            ContainerInfo::Orphan { key: "orphan:src/side.py".to_string() }
        );
    }

    // --- .pyi stub -------------------------------------------------------------

    #[test]
    fn a_pyi_stub_beside_its_py_module_shares_the_key_but_is_never_a_member() {
        let tree = Tree::new("stub");
        tree.write("pkg/mod.py", "");
        tree.write("pkg/mod.pyi", "");
        let context = ProjectContext::load(&tree.0).unwrap();
        assert_eq!(context.container_for(&RelPath::new("pkg/mod.py")), module("pkg.mod", Some("pkg"), "mod"));
        assert_eq!(
            context.container_for(&RelPath::new("pkg/mod.pyi")),
            ContainerInfo::Stub { key: "pkg.mod".to_string() }
        );
    }

    #[test]
    fn an_init_pyi_stub_carries_the_packages_own_key() {
        let tree = Tree::new("init-stub");
        tree.write("pkg/__init__.py", "");
        tree.write("pkg/__init__.pyi", "");
        let context = ProjectContext::load(&tree.0).unwrap();
        assert_eq!(
            context.container_for(&RelPath::new("pkg/__init__.pyi")),
            ContainerInfo::Stub { key: "pkg".to_string() }
        );
    }

    /// A `.pyi`-only module (no `.py` twin at all - a common shape for a C
    /// extension) still gets a stub key, never treated as an ordinary
    /// module.
    #[test]
    fn a_standalone_pyi_with_no_py_twin_is_still_a_stub() {
        let tree = Tree::new("standalone-stub");
        tree.write("pkg/native.pyi", "");
        let context = ProjectContext::load(&tree.0).unwrap();
        assert_eq!(
            context.container_for(&RelPath::new("pkg/native.pyi")),
            ContainerInfo::Stub { key: "pkg.native".to_string() }
        );
    }

    // --- reload on pyproject.toml edit -----------------------------------------

    /// Acceptance: editing `pyproject.toml` and reloading
    /// (`workspaceChanged`'s own effect, exercised directly here - see
    /// `plugins/rust/src/project`'s own equivalent test for why: `run::Session::load_project`
    /// is what actually calls this again after a live edit, and this test
    /// proves the half that matters, that two `load` calls against two
    /// different files disagree the right way).
    #[test]
    fn reloading_after_a_pyproject_toml_edit_picks_up_the_new_root() {
        let tree = Tree::new("reload");
        tree.write("src/pkg/mod.py", "");
        tree.write("other/pkg2/mod.py", "");
        let before = ProjectContext::load(&tree.0).unwrap();
        assert_eq!(before.roots(), &[Root { dir: RelPath::new("src") }]);
        assert!(matches!(
            before.container_for(&RelPath::new("other/pkg2/mod.py")),
            ContainerInfo::Orphan { .. }
        ));

        tree.write(
            "pyproject.toml",
            "[tool.poetry]\npackages = [{ include = \"pkg\", from = \"src\" }, { include = \"pkg2\", from = \"other\" }]\n",
        );
        let after = ProjectContext::load(&tree.0).unwrap();
        let mut dirs: Vec<&str> = after.roots().iter().map(|root| root.dir.as_str()).collect();
        dirs.sort_unstable();
        assert_eq!(dirs, vec!["other", "src"]);
        assert_eq!(
            after.container_for(&RelPath::new("other/pkg2/mod.py")),
            module("pkg2.mod", Some("pkg2"), "mod")
        );
    }

    // --- exclude_dirs ----------------------------------------------------------

    #[test]
    fn a_venv_directory_is_never_walked_into() {
        let tree = Tree::new("venv");
        tree.write("pkg/mod.py", "");
        tree.write(".venv/lib/site-packages/other/__init__.py", "");
        let context = ProjectContext::load(&tree.0).unwrap();
        assert!(matches!(
            context.container_for(&RelPath::new(".venv/lib/site-packages/other/__init__.py")),
            ContainerInfo::Orphan { .. }
        ));
        assert_eq!(context.container_for(&RelPath::new("pkg/mod.py")), module("pkg.mod", Some("pkg"), "mod"));
    }

    // --- has_container (Decision 8) --------------------------------------------

    /// Every package above a module is addressable too, so `import pkg` and
    /// `import pkg.sub` are both "ours" even though only `pkg/sub/deep.py`
    /// exists as a file.
    #[test]
    fn every_package_above_a_module_is_a_container_of_this_project() {
        let tree = Tree::new("has-container");
        tree.write("pkg/sub/deep.py", "");
        let context = ProjectContext::load(&tree.0).unwrap();
        for key in ["pkg", "pkg.sub", "pkg.sub.deep"] {
            assert!(context.has_container(key), "{key} must be one of ours");
        }
    }

    /// The case a prefix match would get wrong: `pkg.subtle` shares the
    /// string `pkg.sub` with nothing it is actually under.
    #[test]
    fn a_name_that_merely_shares_a_string_prefix_is_not_one_of_ours() {
        let tree = Tree::new("has-container-prefix");
        tree.write("pkg/sub/deep.py", "");
        let context = ProjectContext::load(&tree.0).unwrap();
        assert!(!context.has_container("pkg.subtle"));
        assert!(!context.has_container("pkg.su"));
        assert!(!context.has_container("os"), "the standard library is not this project");
    }

    /// A `.pyi` stub contributes no declarations (Decision 6) and still
    /// contributes its key here - see `has_container`'s own doc.
    #[test]
    fn a_stub_only_module_is_still_one_of_ours() {
        let tree = Tree::new("has-container-stub");
        tree.write("pkg/native.pyi", "");
        let context = ProjectContext::load(&tree.0).unwrap();
        assert!(context.has_container("pkg.native"));
    }

    /// An orphan's synthetic key must never make an import look resolvable.
    #[test]
    fn an_orphans_synthetic_key_is_not_a_container_any_import_can_name() {
        let tree = Tree::new("has-container-orphan");
        tree.write("src/pkg/mod.py", "");
        tree.write("tools/generate.py", "");
        let context = ProjectContext::load(&tree.0).unwrap();
        assert!(!context.has_container("orphan:tools/generate.py"));
        assert!(!context.has_container("tools"));
        assert!(context.has_container("pkg.mod"));
    }

    // --- container_for on a path never seen by load ----------------------------

    #[test]
    fn container_for_answers_a_path_load_never_walked() {
        let tree = Tree::new("unseen");
        tree.write("pkg/__init__.py", "");
        let context = ProjectContext::load(&tree.0).unwrap();
        // `pkg/new_module.py` was never on disk during `load`, but the
        // answer must still be exactly what a reload would compute.
        assert_eq!(
            context.container_for(&RelPath::new("pkg/new_module.py")),
            module("pkg.new_module", Some("pkg"), "new_module")
        );
    }
}
