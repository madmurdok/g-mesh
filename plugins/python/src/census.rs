//! GM-314: a test-only census of what the structural tier *declines* to turn
//! into an open site.
//!
//! The whole module is `#[cfg(test)]` and so is every call into it, so nothing
//! here is compiled into the plugin binary. It exists to put a number behind
//! `extractor::bodies`' Decision 7 ("what deliberately does not become an open
//! site") measured on a real corpus, using the extractor's own resolution
//! logic rather than a regex over source text.
//!
//! Run it with:
//!
//! ```text
//! GM314_CORPUS=/path/to/project cargo test -p g-mesh-plugin-python --lib \
//!     census::open_site_census -- --ignored --nocapture
//! ```

use std::cell::RefCell;
use std::collections::BTreeMap;

/// The syntactic position a name was written in. A "declaration is expected
/// here" position is any of the first three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Ctx {
    /// A positional base in `class C(Base)`.
    Base,
    /// The head of a `@decorator` (its call arguments are `Other`).
    Decorator,
    /// A parameter annotation, a return annotation, or an annotated
    /// assignment's type.
    Annotation,
    /// Anything else: an ordinary expression, a call, an argument.
    Other,
}

/// Why the extractor emitted neither an edge nor an open site for a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Reason {
    /// A bare name bound in an enclosing scope: a parameter, a local, a
    /// comprehension variable. Resolved, just not to a node.
    BareLocal,
    /// A bare name that is an imported *module* (`import a.b` then `a`).
    BareImportedModule,
    /// A bare name imported from outside the project (`os`, a distribution).
    BareExternalImport,
    /// A bare name nothing in this file's scopes, declarations or imports
    /// knows: a builtin, a star-import member, an undefined name. **This is
    /// the shape Decision 7 excludes.**
    BareUnknown,
    /// A dotted name whose root is outside the project (`os.path.join`).
    DottedExternal,
    /// A dotted name naming a member of one of this file's own classes that
    /// this file does not declare (an inherited method).
    DottedOwnMissing,
}

#[derive(Debug, Default)]
pub(crate) struct Data {
    /// `(reason, ctx, is_call) -> count`.
    pub counts: BTreeMap<(Reason, Ctx, bool), u64>,
    /// Every `BareUnknown` name, with how often it occurs, split by whether
    /// the file it occurs in has a project-internal star import.
    pub unknown_names_glob: BTreeMap<String, u64>,
    pub unknown_names_plain: BTreeMap<String, u64>,
    /// Files walked, and files carrying at least one project-internal
    /// `from x import *`.
    pub files: u64,
    pub files_with_internal_glob: u64,
    /// Open sites the extractor really emitted, summed over the corpus.
    pub open_sites: u64,
    /// Exactly what `lsp::bridge::questions` would put in `asking`: every open
    /// site except `Implementation`, plus one question per node whose
    /// `nativeKind` is in `implementation_kinds` - which `plugin.toml` sets to
    /// `[]` for this plugin (GM-299 finding 3), so the two are equal here.
    /// This is the number `max_sites` caps.
    pub questions: u64,
    /// Whether the file currently being extracted has an internal glob. Set
    /// by the declaration pass, read when a `BareUnknown` is recorded.
    current_file_has_internal_glob: bool,
}

thread_local! {
    static DATA: RefCell<Data> = RefCell::new(Data::default());
    static CTX: RefCell<Vec<Ctx>> = const { RefCell::new(Vec::new()) };
    static IS_CALL: RefCell<Vec<bool>> = const { RefCell::new(Vec::new()) };
    static ON: RefCell<bool> = const { RefCell::new(false) };
}

pub(crate) fn enable() {
    ON.with(|on| *on.borrow_mut() = true);
}

pub(crate) fn push_ctx(ctx: Ctx) {
    CTX.with(|stack| stack.borrow_mut().push(ctx));
}

pub(crate) fn pop_ctx() {
    CTX.with(|stack| {
        stack.borrow_mut().pop();
    });
}

pub(crate) fn push_call(is_call: bool) {
    IS_CALL.with(|stack| stack.borrow_mut().push(is_call));
}

pub(crate) fn pop_call() {
    IS_CALL.with(|stack| {
        stack.borrow_mut().pop();
    });
}

fn ctx() -> Ctx {
    CTX.with(|stack| stack.borrow().last().copied().unwrap_or(Ctx::Other))
}

fn is_call() -> bool {
    IS_CALL.with(|stack| stack.borrow().last().copied().unwrap_or(false))
}

/// Records one declined name.
pub(crate) fn record(reason: Reason, name: &str) {
    if !ON.with(|on| *on.borrow()) {
        return;
    }
    let (ctx, is_call) = (ctx(), is_call());
    DATA.with(|data| {
        let mut data = data.borrow_mut();
        *data.counts.entry((reason, ctx, is_call)).or_default() += 1;
        if reason == Reason::BareUnknown {
            let bucket = if data.current_file_has_internal_glob {
                &mut data.unknown_names_glob
            } else {
                &mut data.unknown_names_plain
            };
            *bucket.entry(format!("{ctx:?}\t{name}")).or_default() += 1;
        }
    });
}

/// Called by the declaration pass for every `from x import *`.
pub(crate) fn note_glob(internal: bool) {
    if !ON.with(|on| *on.borrow()) {
        return;
    }
    DATA.with(|data| {
        let mut data = data.borrow_mut();
        if internal {
            data.current_file_has_internal_glob = true;
        }
    });
}

/// Called by the census driver between files, once the *declaration* pass has
/// run and before the body pass, so `note_glob` has already fired.
pub(crate) fn start_file() {
    DATA.with(|data| data.borrow_mut().current_file_has_internal_glob = false);
}

pub(crate) fn end_file(open_sites: u64, questions: u64, had_internal_glob_out: &mut bool) {
    DATA.with(|data| {
        let mut data = data.borrow_mut();
        data.files += 1;
        data.open_sites += open_sites;
        data.questions += questions;
        if data.current_file_has_internal_glob {
            data.files_with_internal_glob += 1;
            *had_internal_glob_out = true;
        }
    });
}

pub(crate) fn with_data<R>(f: impl FnOnce(&Data) -> R) -> R {
    DATA.with(|data| f(&data.borrow()))
}

// ---------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------

#[cfg(test)]
mod run {
    use std::path::PathBuf;

    use g_mesh_plugin_sdk::{walk_project, Extractor, OpenSiteKind, RelPath};

    use super::*;
    use crate::extractor::PythonExtractor;
    use crate::project::ProjectContext;

    #[test]
    #[ignore = "GM-314 measurement; needs GM314_CORPUS"]
    fn open_site_census() {
        let root = PathBuf::from(std::env::var("GM314_CORPUS").expect("GM314_CORPUS"));
        let project = ProjectContext::load(&root).expect("load");
        // Exactly the manifest's own extensions and exclusions, so the census
        // walks the file set `--bulk-index` would.
        let extensions = [".py".to_string(), ".pyi".to_string()];
        let exclude: Vec<String> =
            [".venv", "venv", "__pycache__", ".tox", ".mypy_cache", "site-packages", "node_modules"]
                .iter()
                .map(|dir| (*dir).to_string())
                .collect();
        let files = walk_project(&root, &extensions, &exclude);
        enable();

        let mut extracted = 0u64;
        let mut glob_files: Vec<String> = Vec::new();
        let mut declared: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for path in &files {
            let Ok(source) = std::fs::read_to_string(root.join(path.as_str())) else { continue };
            start_file();
            let graph = PythonExtractor.extract(&project, &RelPath::new(path.as_str()), &source);
            let mut had_glob = false;
            // `lsp::bridge::questions`, reproduced. `implementation_kinds` is
            // empty for Python, so the only exclusion is `Implementation`
            // sites - of which this extractor emits none.
            let implementations =
                graph.open_sites.iter().filter(|site| site.kind == OpenSiteKind::Implementation).count()
                    as u64;

            // GM-314: every name this project DECLARES, so the analysis can
            // ask the question that actually matters - not "is this a
            // builtin" but "could any engine answer this with a node this
            // index holds". Placeholders are excluded: they are addresses
            // waiting on a declaration, not declarations.
            for node in &graph.nodes {
                let placeholder = matches!(
                    node.native_kind.as_deref(),
                    Some("pending_symbol" | "reexport" | "resolved_module" | "external_module")
                );
                if !placeholder && !node.name.is_empty() {
                    declared.insert(node.name.clone());
                }
            }
            let sites = graph.open_sites.len() as u64;
            end_file(sites, sites - implementations, &mut had_glob);
            if had_glob {
                glob_files.push(path.as_str().to_string());
            }
            extracted += 1;
        }

        with_data(|data| {
            println!("CENSUS-META\tcorpus\t{}", root.display());
            println!("CENSUS-META\twalked_files\t{}", files.len());
            println!("CENSUS-META\textracted_files\t{extracted}");
            println!("CENSUS-META\tfiles_with_internal_glob\t{}", data.files_with_internal_glob);
            println!("CENSUS-META\tcurrent_open_sites\t{}", data.open_sites);
            println!("CENSUS-META\tbridge_questions_asking\t{}", data.questions);
            for ((reason, ctx, is_call), count) in &data.counts {
                println!("CENSUS-COUNT\t{reason:?}\t{ctx:?}\tis_call={is_call}\t{count}");
            }
            let glob_total: u64 = data.unknown_names_glob.values().sum();
            let plain_total: u64 = data.unknown_names_plain.values().sum();
            println!("CENSUS-META\tbare_unknown_in_glob_files\t{glob_total}");
            println!("CENSUS-META\tbare_unknown_in_plain_files\t{plain_total}");
            let dir = std::env::var("GM314_OUT").unwrap_or_else(|_| "/tmp".to_string());
            let mut lines = String::new();
            for (name, count) in &data.unknown_names_glob {
                lines.push_str(&format!("glob\t{name}\t{count}\n"));
            }
            for (name, count) in &data.unknown_names_plain {
                lines.push_str(&format!("plain\t{name}\t{count}\n"));
            }
            std::fs::write(format!("{dir}/python-unknown-names.tsv"), lines).unwrap();
            println!("CENSUS-META\tname_histogram\t{dir}/python-unknown-names.tsv");
            std::fs::write(format!("{dir}/python-glob-files.txt"), glob_files.join("\n")).unwrap();
            std::fs::write(
                format!("{dir}/python-declared-names.txt"),
                declared.iter().cloned().collect::<Vec<_>>().join("\n"),
            )
            .unwrap();
            println!("CENSUS-META\tdeclared_names\t{}", declared.len());
        });
    }
}
