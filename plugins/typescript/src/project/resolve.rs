//! Module-specifier resolution: which indexed file an import names.
//!
//! Four disjoint kinds, each ending in the same candidate pipeline against
//! the existence set:
//!
//! - **relative** (`./x`, `../x`, `.`, `..`): path arithmetic from the
//!   importer's directory;
//! - **`#private`**: the `imports` map of the importer's nearest
//!   package.json, never a grandparent's;
//! - **bare**, first as a workspace package (`@scope/math` -> its
//!   directory), then as a tsconfig `paths` alias (`@/utils`). A specifier
//!   the workspace resolves never reaches the alias map.
//!
//! Everything else (`react`, `node:fs`) stays unresolved: none of its files
//! is indexed.
//!
//! A candidate counts only when it is in the existence set, the files the
//! SDK walk indexes plus presence updates. So a gitignored or `dist/` build
//! output never shadows the source it was built from, and a file this plugin
//! does not parse (`./styles.css`) is never claimed.

use std::collections::BTreeSet;

use g_mesh_plugin_sdk::RelPath;

use crate::extractor::grammar::grammar_for;
use crate::project::exports::imports_targets;
use crate::project::paths;
use crate::project::tsconfig::expand_paths_candidates;
use crate::project::workspace::{package_entry_targets, parse_bare_specifier};
use crate::project::TsProject;

/// Extensions tried after a specifier that has none, in TypeScript's order
/// (its own before the JS ones).
pub const EXTENSIONS: [&str; 9] = [".ts", ".tsx", ".d.ts", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"];

/// TypeScript's ESM rule: a specifier with a JS extension names the emitted
/// file, so the source is the matching TS file (`./x.js` -> `x.ts`).
pub const TS_SUBSTITUTIONS: [(&str, &[&str]); 4] = [
    (".js", &[".ts", ".tsx", ".d.ts"]),
    (".jsx", &[".tsx"]),
    (".mjs", &[".mts", ".d.mts"]),
    (".cjs", &[".cts", ".d.cts"]),
];

/// The address a relative specifier naming the project root itself has
/// (`..` in `src/a.ts`, `.` or `./` in `main.ts`): [`paths::normalize`]'s
/// spelling of the empty path.
pub const ROOT_ADDRESS: &str = ".";

/// `./x`, `../x`, `.` and `..`; everything else is bare or `#private`.
pub fn is_relative_specifier(specifier: &str) -> bool {
    specifier == "." || specifier == ".." || specifier.starts_with("./") || specifier.starts_with("../")
}

/// The last extension of `path`'s file name, lowercased (`a.d.ts` -> `.ts`);
/// `""` for none or a dotfile name.
pub fn extension_of(path: &str) -> String {
    let name = paths::basename(path);
    match name.rfind('.') {
        Some(at) if at > 0 => name[at..].to_lowercase(),
        _ => String::new(),
    }
}

/// Every path `base` could name, most specific first: TS substitutions for a
/// JS extension (then the JS file itself), `base` alone when it already
/// names a parsed source file, otherwise `base` plus each of [`EXTENSIONS`],
/// then `base/index` plus each. The root's own address `.` names a directory
/// whose file form would sit outside the project, so only its `index` files
/// count (`index.ts`, `index.tsx`, ...).
pub fn candidate_paths(base: &str) -> Vec<String> {
    if base == ROOT_ADDRESS {
        return EXTENSIONS.iter().map(|extension| format!("index{extension}")).collect();
    }
    let extension = extension_of(base);
    if let Some((_, substitutions)) = TS_SUBSTITUTIONS.iter().find(|(from, _)| *from == extension) {
        let stem = &base[..base.len() - extension.len()];
        let mut candidates: Vec<String> = substitutions.iter().map(|to| format!("{stem}{to}")).collect();
        candidates.push(base.to_string());
        return candidates;
    }
    if grammar_for(&RelPath::new(base)).is_some() {
        return vec![base.to_string()];
    }
    EXTENSIONS
        .iter()
        .map(|extension| format!("{base}{extension}"))
        .chain(EXTENSIONS.iter().map(|extension| format!("{base}/index{extension}")))
        .collect()
}

/// The first candidate of any of `bases` that exists.
fn first_existing<I>(existence: &BTreeSet<RelPath>, bases: I) -> Option<RelPath>
where
    I: IntoIterator<Item = String>,
{
    bases
        .into_iter()
        .flat_map(|base| candidate_paths(&base))
        .map(RelPath::new)
        .find(|candidate| existence.contains(candidate))
}

/// The importer's directory, or `None` when `from` climbs out of the root.
fn importer_dir(from: &RelPath) -> Option<&str> {
    let from = from.as_str();
    if from.split('/').any(|segment| segment == "..") || from.starts_with('/') {
        return None;
    }
    Some(paths::dirname(from))
}

/// A relative specifier written in `from`. `None` for a non-relative one, a
/// path leaving the root, or no existing candidate. One naming the root
/// itself resolves to the root's `index` file ([`candidate_paths`]).
pub fn resolve_relative(project: &TsProject, specifier: &str, from: &RelPath) -> Option<RelPath> {
    if !is_relative_specifier(specifier) {
        return None;
    }
    let base = paths::normalize(&paths::join(importer_dir(from)?, specifier));
    if base != ROOT_ADDRESS && paths::escapes(&base) {
        return None;
    }
    first_existing(&project.existence, [base])
}

/// Every address other than `path` itself whose candidates
/// ([`candidate_paths`]) include `path`, sorted: the bases an unresolved
/// relative import may be waiting at ([`relative_address`]) that the new
/// file `path` can answer. `src/missing.ts` -> `src/missing` (and
/// `src/missing.js`, through [`TS_SUBSTITUTIONS`]); `src/missing/index.ts`
/// -> `src/missing` too; a root `index.ts` -> [`ROOT_ADDRESS`]. Which
/// existing candidate wins is still the resolver's, on re-extract; this only
/// names who to ask.
pub fn addresses_resolving_to(path: &str) -> Vec<String> {
    let mut bases: BTreeSet<String> = BTreeSet::new();
    for extension in EXTENSIONS {
        if let Some(stem) = path.strip_suffix(extension) {
            bases.insert(stem.to_string());
            if let Some(dir) = stem.strip_suffix("/index") {
                bases.insert(dir.to_string());
            } else if stem == "index" {
                bases.insert(ROOT_ADDRESS.to_string());
            }
        }
    }
    for (js_extension, substitutions) in TS_SUBSTITUTIONS {
        for substitution in substitutions {
            if let Some(stem) = path.strip_suffix(substitution) {
                bases.insert(format!("{stem}{js_extension}"));
            }
        }
    }
    bases.remove(path);
    bases.retain(|base| !base.is_empty() && candidate_paths(base).iter().any(|candidate| candidate == path));
    bases.into_iter().collect()
}

/// For each address of `path` ([`addresses_resolving_to`]), the candidate
/// that answered it before `path` existed: the first candidate after `path`
/// in [`candidate_paths`]' order that is in `existence`, sorted and deduplicated.
/// An import linked there, by any specifier kind, now resolves to `path`
/// instead (`./m` on `src/m.tsx` or `src/m/index.ts` once `src/m.ts`
/// exists). Only the previous winner is named: a candidate further down
/// never answered that address, and `path` itself or an earlier candidate
/// still wins over it. Which file wins stays the resolver's, on re-extract.
pub fn candidates_shadowed_by(path: &str, existence: &BTreeSet<RelPath>) -> Vec<String> {
    let mut shadowed: BTreeSet<String> = BTreeSet::new();
    for address in addresses_resolving_to(path) {
        let candidates = candidate_paths(&address);
        let Some(at) = candidates.iter().position(|candidate| candidate == path) else {
            continue;
        };
        if let Some(previous) = candidates[at + 1..]
            .iter()
            .find(|candidate| existence.contains(&RelPath::new(candidate.as_str())))
        {
            shadowed.insert(previous.clone());
        }
    }
    shadowed.into_iter().collect()
}

/// The project-relative path a relative specifier written in `from` names,
/// before any extension or `index` candidate is tried (`./missing` in
/// `src/a.ts` -> `src/missing`); `None` for a non-relative one. Pure path
/// arithmetic: the result may name no file, the root (`.`), or a path above
/// it (`../x`).
pub fn relative_address(specifier: &str, from: &RelPath) -> Option<String> {
    is_relative_specifier(specifier)
        .then(|| paths::normalize(&paths::join(paths::dirname(from.as_str()), specifier)))
}

/// A bare specifier naming a workspace package or a subpath of one. Does not
/// depend on the importer: a package name means the same everywhere.
pub fn resolve_workspace(project: &TsProject, specifier: &str) -> Option<RelPath> {
    if project.packages.is_empty() {
        return None;
    }
    let (name, subpath) = parse_bare_specifier(specifier)?;
    let package = project.packages.get(&name)?;
    first_existing(&project.existence, package_entry_targets(package, &subpath))
}

/// A bare specifier expanded by the `paths` of the config nearest `from`.
pub fn resolve_tsconfig_paths(project: &TsProject, specifier: &str, from: &RelPath) -> Option<RelPath> {
    if is_relative_specifier(specifier) {
        return None;
    }
    let config = project.tsconfig_for(importer_dir(from)?)?;
    first_existing(&project.existence, expand_paths_candidates(config, specifier))
}

/// A `#` specifier through the `imports` map of `from`'s nearest
/// package.json. A nearest package.json without `imports` leaves it
/// unresolved; the walk never falls through to a grandparent.
pub fn resolve_package_imports(project: &TsProject, specifier: &str, from: &RelPath) -> Option<RelPath> {
    if !specifier.starts_with('#') {
        return None;
    }
    let config = project.imports_for(importer_dir(from)?)?;
    first_existing(&project.existence, imports_targets(config, specifier))
}

/// The file `specifier`, written in `from`, names: relative first, then
/// `#private`, then workspace before tsconfig `paths`.
pub fn resolve(project: &TsProject, specifier: &str, from: &RelPath) -> Option<RelPath> {
    if is_relative_specifier(specifier) {
        return resolve_relative(project, specifier, from);
    }
    if specifier.starts_with('#') {
        return resolve_package_imports(project, specifier, from);
    }
    resolve_workspace(project, specifier).or_else(|| resolve_tsconfig_paths(project, specifier, from))
}
