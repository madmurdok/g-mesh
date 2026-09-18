//! GM-314: a test-only census of what the structural tier *declines* to turn
//! into an open site.
//!
//! The whole module is `#[cfg(test)]` and so is every call into it, so nothing
//! here is compiled into the plugin binary. It puts a number behind
//! `extractor::bodies`' Decision 7 ("deliberately **not** recorded for an
//! unresolved *type* reference … `Vec`, `String`, `Option` … would swamp a
//! bridge") measured on a real corpus, using the extractor's own resolution
//! logic rather than a regex over source text.
//!
//! Run it with:
//!
//! ```text
//! GM314_CORPUS=/path/to/crate cargo test -p g-mesh-plugin-rust --lib \
//!     census::run::open_site_census -- --ignored --nocapture
//! ```

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

/// The syntactic position a name was written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Ctx {
    /// A trait in `impl Tr for T`, or a supertrait in `trait Sub: Super`.
    Supertype,
    /// The macro path of an `#[attribute]` - `derive`, `tokio::test`.
    AttrHead,
    /// A name inside `#[derive(...)]`: `Serialize`, `Debug`. The only
    /// attribute argument that is a symbol rather than a predicate.
    AttrDeriveArg,
    /// A name inside `#[cfg(...)]`/`#[cfg_attr(...)]`: `feature`,
    /// `target_os`, `unix`. Conditional-compilation predicates and their
    /// values - not symbols at all, and nothing any engine could resolve.
    AttrCfgArg,
    /// A name inside any other attribute's token tree.
    AttrOtherArg,
    /// Anything else; `Reason` already separates type position from value
    /// position, which is what a Rust annotation is.
    Other,
}

/// Why the extractor emitted neither an edge nor an open site for a name -
/// except the two `Open*` variants, which are what it *does* emit today and
/// are counted so the census can be checked against the real open-site total.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Reason {
    /// A bare name bound in an enclosing scope: a local, a generic parameter.
    BareLocal,
    /// `Self` outside a position this tier resolves.
    BareSelf,
    /// A bare name `use`d from a crate this project does not model.
    BareExternalImport,
    /// A bare name nothing here declares or imports, in **type** position.
    /// `Vec`, `String`, `Option` - the shape Decision 7 names and excludes.
    BareUnknownType,
    /// The same, in **value** position: a constant or enum variant reached
    /// through a glob import or the prelude. Also excluded today.
    BareUnknownValue,
    /// A dotted path rooted at a crate this project does not model.
    DottedExternal,
    /// Already an open site: an unresolved bare **call**.
    OpenUnresolvedCall,
    /// Already an open site: an unresolved multi-segment path.
    OpenUnresolvedPath,
}

#[derive(Debug, Default)]
pub(crate) struct Data {
    pub counts: BTreeMap<(Reason, Ctx), u64>,
    pub unknown_type_names: BTreeMap<String, u64>,
    pub unknown_value_names: BTreeMap<String, u64>,
    pub attr_names: BTreeMap<String, u64>,
    pub files: u64,
    pub open_sites: u64,
    /// Exactly what `lsp::bridge::questions` would put in `asking`: every open
    /// site except `Implementation`, plus one `implementation` question per
    /// node whose `nativeKind` is in the manifest's `implementation_kinds`
    /// (`["trait"]` for this plugin). This is the number `max_sites` caps.
    pub questions: u64,
    pub implementation_sites: u64,
    /// Occurrences of an unresolved name in a module that has a glob `use` in
    /// scope - `mod tests { use super::*; }` above all - split by whether the
    /// project declares that name anywhere. A glob is the only way a name the
    /// project *does* declare can reach a module without the structural tier
    /// seeing it, so this separates "the answer is in the index and a glob
    /// hid it" from "the answer was never in the index".
    pub unresolved_under_glob: u64,
    pub unresolved_not_under_glob: u64,
    /// Module keys of the file being extracted that carry a glob `use`.
    globbed_modules: BTreeSet<String>,
}

thread_local! {
    static DATA: RefCell<Data> = RefCell::new(Data::default());
    static CTX: RefCell<Vec<Ctx>> = const { RefCell::new(Vec::new()) };
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

fn ctx() -> Ctx {
    CTX.with(|stack| stack.borrow().last().copied().unwrap_or(Ctx::Other))
}

pub(crate) fn record(reason: Reason, name: &str, module_key: &str) {
    if !ON.with(|on| *on.borrow()) {
        return;
    }
    let ctx = ctx();
    DATA.with(|data| {
        let mut data = data.borrow_mut();
        *data.counts.entry((reason, ctx)).or_default() += 1;
        if matches!(ctx, Ctx::AttrHead | Ctx::AttrDeriveArg | Ctx::AttrCfgArg | Ctx::AttrOtherArg) {
            *data.attr_names.entry(format!("{ctx:?}\t{name}")).or_default() += 1;
            return;
        }
        let globbed = data
            .globbed_modules
            .iter()
            .any(|glob| module_key == glob || module_key.starts_with(&format!("{glob}::")));
        match reason {
            Reason::BareUnknownType => {
                *data.unknown_type_names.entry(format!("{ctx:?}\t{name}")).or_default() += 1;
            }
            Reason::BareUnknownValue => {
                *data.unknown_value_names.entry(format!("{ctx:?}\t{name}")).or_default() += 1;
            }
            _ => return,
        }
        if globbed {
            data.unresolved_under_glob += 1;
        } else {
            data.unresolved_not_under_glob += 1;
        }
    });
}

/// Called by the declaration pass for every `use ...::*` that resolves to a
/// container inside this project.
pub(crate) fn note_glob(module_key: &str) {
    if !ON.with(|on| *on.borrow()) {
        return;
    }
    DATA.with(|data| {
        data.borrow_mut().globbed_modules.insert(module_key.to_string());
    });
}

pub(crate) fn start_file() {
    DATA.with(|data| data.borrow_mut().globbed_modules.clear());
}

pub(crate) fn end_file(open_sites: u64, questions: u64, implementation_sites: u64) {
    DATA.with(|data| {
        let mut data = data.borrow_mut();
        data.files += 1;
        data.open_sites += open_sites;
        data.questions += questions;
        data.implementation_sites += implementation_sites;
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

    use g_mesh_plugin_sdk::{walk_project, Extractor, RelPath};

    use super::*;
    use crate::extractor::RustExtractor;
    use crate::project::ProjectContext;

    #[test]
    #[ignore = "GM-314 measurement; needs GM314_CORPUS"]
    fn open_site_census() {
        let root = PathBuf::from(std::env::var("GM314_CORPUS").expect("GM314_CORPUS"));
        let project = ProjectContext::load(&root).expect("load");
        // Exactly the manifest's own extensions and exclusions.
        let files = walk_project(&root, &[".rs".to_string()], &["target".to_string()]);
        enable();

        let mut extracted = 0u64;
        let mut declared: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for path in &files {
            let Ok(source) = std::fs::read_to_string(root.join(path.as_str())) else { continue };
            start_file();
            let graph = RustExtractor.extract(&project, &RelPath::new(path.as_str()), &source);
            // `lsp::bridge::questions`, reproduced: `Implementation` sites are
            // counted as unanswerable and never asked, and `plugin.toml` sets
            // `implementation_kinds = ["trait"]`, which adds one question per
            // trait node.
            let implementations = graph
                .open_sites
                .iter()
                .filter(|site| site.kind == g_mesh_plugin_sdk::OpenSiteKind::Implementation)
                .count() as u64;
            let traits =
                graph.nodes.iter().filter(|node| node.native_kind.as_deref() == Some("trait")).count() as u64;

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
            end_file(sites, sites - implementations + traits, implementations);
            extracted += 1;
        }

        with_data(|data| {
            println!("CENSUS-META\tcorpus\t{}", root.display());
            println!("CENSUS-META\twalked_files\t{}", files.len());
            println!("CENSUS-META\textracted_files\t{extracted}");
            println!("CENSUS-META\tcurrent_open_sites\t{}", data.open_sites);
            println!("CENSUS-META\timplementation_sites_never_asked\t{}", data.implementation_sites);
            println!("CENSUS-META\tbridge_questions_asking\t{}", data.questions);
            println!("CENSUS-META\tunresolved_under_glob\t{}", data.unresolved_under_glob);
            println!("CENSUS-META\tunresolved_not_under_glob\t{}", data.unresolved_not_under_glob);
            for ((reason, ctx), count) in &data.counts {
                println!("CENSUS-COUNT\t{reason:?}\t{ctx:?}\t{count}");
            }
            let dir = std::env::var("GM314_OUT").unwrap_or_else(|_| "/tmp".to_string());
            let dump = |name: &str, map: &BTreeMap<String, u64>| {
                let mut lines = String::new();
                for (key, count) in map {
                    lines.push_str(&format!("{key}\t{count}\n"));
                }
                std::fs::write(format!("{dir}/rust-{name}.tsv"), lines).unwrap();
                let total: u64 = map.values().sum();
                println!("CENSUS-META\t{name}\tdistinct={} occurrences={total}", map.len());
            };
            dump("unknown-type-names", &data.unknown_type_names);
            dump("unknown-value-names", &data.unknown_value_names);
            dump("attr-names", &data.attr_names);
            std::fs::write(
                format!("{dir}/rust-declared-names.txt"),
                declared.iter().cloned().collect::<Vec<_>>().join("\n"),
            )
            .unwrap();
            println!("CENSUS-META\tdeclared_names\t{}", declared.len());
        });
    }
}
