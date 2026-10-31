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
use crate::project::exports::{
    imports_base, imports_specifiers_targeting, imports_targets, key_targets, match_wildcard, PackageImports,
};
use crate::project::paths;
use crate::project::tsconfig::{expand_paths_candidates, specifiers_targeting, EffectiveConfig};
use crate::project::workspace::{
    package_entry_targets, package_may_name, parse_bare_specifier, subpaths_targeting, WorkspacePackage,
};
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
pub const ROOT_ADDRESS: &str = paths::ROOT;

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

/// Whether `base` names a file as written, so [`candidate_paths`] tries no
/// extension or `index` file for it: it has a JS extension with TS
/// substitutions, or one this plugin parses.
pub fn names_file(base: &str) -> bool {
    let extension = extension_of(base);
    TS_SUBSTITUTIONS.iter().any(|(from, _)| *from == extension) || grammar_for(&RelPath::new(base)).is_some()
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

/// The candidate of `bases` that answered before `path` existed, if `path`
/// now wins over it: the first candidate after `path`'s first place in the
/// list [`first_existing`] walks that is in `existence`. `None` when `path`
/// is not a candidate of `bases` or nothing after it exists. An existing
/// candidate before `path` is not consulted: in a batch create it may be
/// another new file, and the file it shadowed must still be named by
/// `path`'s own event. The list is walked lazily, base by base.
fn displaced_by<I>(path: &str, existence: &BTreeSet<RelPath>, bases: I) -> Option<String>
where
    I: IntoIterator<Item = String>,
{
    let mut candidates = bases.into_iter().flat_map(|base| candidate_paths(&base));
    candidates.by_ref().find(|candidate| candidate == path)?;
    candidates.find(|candidate| candidate != path && existence.contains(&RelPath::new(candidate.as_str())))
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

/// The files that answered an alias, workspace package or `#` specifier
/// before `path` existed and that `path` now wins over, sorted and
/// deduplicated: the multi-base counterpart of [`candidates_shadowed_by`].
/// `paths` targets `["src/*", "generated/*"]` and a new `src/x.ts` name
/// `generated/x.ts`, the file `@/x` was linked to.
///
/// The specifiers asked about come from inverting each target pattern
/// against `path` and its addresses ([`addresses_resolving_to`]):
/// [`specifiers_targeting`] for every tsconfig, [`subpaths_targeting`] for
/// every workspace package, [`imports_specifiers_targeting`] for every
/// `imports` map. Every config is asked, not only the one nearest some
/// importer, so this may name a file no importer reaches through it; it
/// never misses one the inversion finds. The order and the previous winner
/// come from the resolver's own list functions ([`displaced_by`]). A
/// workspace specifier's list is the package's entries followed by each
/// config's expansion of the same bare specifier, the order [`resolve`]
/// tries them in, so a new package file displaces an alias answer too. A
/// package whose entries cannot name `path` is skipped
/// ([`package_may_name`]).
pub fn fallbacks_shadowed_by(project: &TsProject, path: &str) -> Vec<String> {
    let existence = &project.existence;
    let Derivation { configs, specifiers } = derive_specifiers(project, path);
    let mut shadowed: BTreeSet<String> = BTreeSet::new();
    for derived in specifiers {
        match derived {
            Derived::Paths(config, specifier) => {
                shadowed.extend(displaced_by(path, existence, expand_paths_candidates(config, &specifier)));
            }
            Derived::Package(package, subpath, specifier) => {
                let entries = package_entry_targets(package, &subpath);
                if configs.is_empty() {
                    shadowed.extend(displaced_by(path, existence, entries));
                    continue;
                }
                for config in &configs {
                    let aliased = expand_paths_candidates(config, &specifier);
                    shadowed.extend(displaced_by(path, existence, entries.iter().cloned().chain(aliased)));
                }
            }
            Derived::Imports(config, specifier) => {
                shadowed.extend(displaced_by(path, existence, imports_targets(config, &specifier)));
            }
        }
    }
    shadowed.into_iter().collect()
}

/// The addresses an unresolved alias, workspace package or `#` import that
/// `path` can answer is waiting at, sorted and deduplicated: for every
/// specifier [`fallbacks_shadowed_by`]'s inversion derives from `path`, its
/// [`waiting_address`] under the config that derived it. A tsconfig's
/// `["src/*", "generated/*"]` and a new `generated/x.ts` give `src/x`, the
/// address an unresolved `@/x` was recorded at ([`miss_address`]). Both sides
/// compute the address with [`waiting_address`]; every config is asked, so
/// the one nearest the importer is among them. A wildcard entry whose target
/// has no `*` derives no specifier, so its target is offered directly when it
/// names `path` ([`static_targets_naming`]).
///
/// An address other than `path` that exists is dropped: no import waits
/// unresolved at a base whose first candidate exists, and core would answer
/// such a key with every importer linked to that file. `path` itself is
/// always kept; core selects only the importers waiting at it.
pub fn waiting_addresses_for(project: &TsProject, path: &str) -> Vec<String> {
    let Derivation { configs, specifiers } = derive_specifiers(project, path);
    let mut addresses: BTreeSet<String> = BTreeSet::new();
    for derived in &specifiers {
        match derived {
            Derived::Paths(config, specifier) => {
                addresses.extend(waiting_address(project, specifier, Some(config), None));
            }
            Derived::Package(_, _, specifier) => {
                if configs.is_empty() {
                    addresses.extend(waiting_address(project, specifier, None, None));
                }
                for config in &configs {
                    addresses.extend(waiting_address(project, specifier, Some(config), None));
                }
            }
            Derived::Imports(config, specifier) => {
                addresses.extend(waiting_address(project, specifier, None, Some(config)));
            }
        }
    }
    addresses.extend(static_targets_naming(project, &configs, path));
    addresses
        .into_iter()
        .filter(|address| address == path || !project.existence.contains(&RelPath::new(address.as_str())))
        .collect()
}

/// The static targets (no `*`) of wildcard `paths` entries of `configs`
/// (the catch-all `"*"` excluded, as in [`waiting_address`]) and of wildcard
/// `imports` keys that equal `path` or one of its addresses
/// ([`addresses_resolving_to`]), resolved the way the forward lookup
/// resolves them. Such an entry names that one path for every capture, so a
/// specifier it matches waits there.
fn static_targets_naming(project: &TsProject, configs: &[&EffectiveConfig], path: &str) -> Vec<String> {
    let mut addresses = addresses_resolving_to(path);
    addresses.push(path.to_string());
    let mut targets: Vec<String> = Vec::new();
    for config in configs {
        for entry in &config.paths {
            if !entry.pattern.contains('*') || entry.pattern == CATCH_ALL_PATTERN {
                continue;
            }
            for target in entry.targets.iter().filter(|target| !target.contains('*')) {
                targets.extend(paths::inside_or_root(&config.resolve_dir, target));
            }
        }
    }
    for config in project.imports_by_dir.values().flatten() {
        for (key, value) in &config.imports {
            if !key.contains('*') {
                continue;
            }
            for target in key_targets(value, None).into_iter().filter(|target| !target.contains('*')) {
                targets.extend(imports_base(&config.dir, &target));
            }
        }
    }
    targets.retain(|target| addresses.contains(target));
    targets
}

/// Every distinct tsconfig of the project (configs shared by several
/// directories counted once).
fn distinct_configs(project: &TsProject) -> Vec<&EffectiveConfig> {
    let mut configs: Vec<&EffectiveConfig> = Vec::new();
    for config in project.tsconfig_by_dir.values().flatten() {
        if !configs.iter().any(|seen| std::ptr::eq(*seen, config.as_ref())) {
            configs.push(config.as_ref());
        }
    }
    configs
}

/// What a new `path` offers through `paths` targets that climb out of their
/// capture (`"@m/*": ["./*/..", "./gen/*"]`): the importers such a target
/// may move onto `path`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ClimbingKeys {
    /// Target prefixes: every import whose target (placeholder address or
    /// linked file) starts with one may now resolve to `path`. Never empty.
    pub prefixes: Vec<String>,
    /// Exact addresses, from exact entries the climbing family matches.
    pub exact: Vec<String>,
}

/// The keys of [`ClimbingKeys`] for `path`. A wildcard target `P1*S1` whose
/// `S1` climbs `k >= 1` segments after the `*`'s own (`./*/..`,
/// `./src/*/../x`) names one address for a whole family of specifiers:
/// `@m/lib/<y>` reaches `lib` for every `<y>`. No finite list of exact keys
/// names that family, so for `path` and each of its addresses
/// ([`addresses_resolving_to`]) the family's fixed specifier prefix `F` is
/// worked out (`@m/lib/`), and every other wildcard target of the same
/// config gives the target prefix its expansion of `F` starts with
/// (`./gen/*` -> `gen/lib/`). An entry whose pattern is more specific than
/// `F` gives its whole target prefix (an over-approximation); an exact
/// entry whose pattern starts with `F` gives its targets as exact keys.
///
/// Empty for a config without such a target, so an ordinary create adds no
/// prefix selector. A prefix naming the project root itself (which would
/// select every import) is never offered.
pub fn climbing_target_prefixes(project: &TsProject, path: &str) -> ClimbingKeys {
    let mut addresses = addresses_resolving_to(path);
    addresses.push(path.to_string());
    let mut keys = ClimbingKeys::default();
    for config in distinct_configs(project) {
        for entry in &config.paths {
            let Some((family_head, _)) = entry.pattern.split_once('*') else {
                continue;
            };
            for target in &entry.targets {
                let Some(climb) = Climb::of(&config.resolve_dir, target) else {
                    continue;
                };
                for address in &addresses {
                    let Some(capture_head) = climb.capture_head(address) else {
                        continue;
                    };
                    let fixed = if capture_head.is_empty() {
                        family_head.to_string()
                    } else {
                        format!("{family_head}{capture_head}/")
                    };
                    offer_family(config, &fixed, &mut keys);
                }
            }
        }
    }
    keys
}

/// The keys every non-climbing target of `config` offers for the
/// specifiers starting with `fixed` ([`climbing_target_prefixes`]).
fn offer_family(config: &EffectiveConfig, fixed: &str, keys: &mut ClimbingKeys) {
    for entry in &config.paths {
        let Some((head, _)) = entry.pattern.split_once('*') else {
            if entry.pattern.starts_with(fixed) {
                for target in &entry.targets {
                    if let Some(exact) = paths::inside_or_root(&config.resolve_dir, target) {
                        push_new(&mut keys.exact, exact);
                    }
                }
            }
            continue;
        };
        let rest = if let Some(rest) = fixed.strip_prefix(head) {
            rest
        } else if head.starts_with(fixed) {
            ""
        } else {
            continue;
        };
        for target in &entry.targets {
            if Climb::of(&config.resolve_dir, target).is_some() {
                continue;
            }
            let Some((target_head, _)) = target.split_once('*') else {
                continue;
            };
            let text = paths::with_forward_slashes(&format!("{target_head}{rest}"));
            if paths::is_rooted(&text) {
                continue;
            }
            let normalized = paths::normalize(&paths::join(&config.resolve_dir, &text));
            if normalized == paths::ROOT || paths::escapes(&normalized) {
                continue;
            }
            let prefix = if text.ends_with('/') { format!("{normalized}/") } else { normalized };
            push_new(&mut keys.prefixes, prefix);
        }
    }
}

fn push_new(list: &mut Vec<String>, value: String) {
    if !list.contains(&value) {
        list.push(value);
    }
}

/// A `paths` target `P1*S1` whose `S1` climbs out of the capture: after
/// the `*`'s own segment, `S1` normalizes to `k >= 1` leading `..`
/// segments and a remainder.
struct Climb {
    /// `resolve_dir/P1` spelled so that `head + capture` is the substituted
    /// path: the normalized directory (with its `/`, or empty for the root)
    /// and the text before the `*` in its segment.
    head: String,
    /// The normalized remainder after the `..` segments; empty for none.
    rest: String,
}

impl Climb {
    fn of(resolve_dir: &str, target: &str) -> Option<Climb> {
        let target = paths::with_forward_slashes(target);
        let (before, after) = target.split_once('*')?;
        let (_, after_segment) = after.split_once('/')?;
        let tail = paths::normalize(after_segment);
        let mut segments: Vec<&str> = tail.split('/').collect();
        let climbs = segments.iter().take_while(|segment| **segment == "..").count();
        if climbs == 0 {
            return None;
        }
        segments.drain(..climbs);
        let (dir, partial) = before.rsplit_once('/').unwrap_or(("", before));
        let dir = paths::normalize(&paths::join(resolve_dir, dir));
        let head = if dir == paths::ROOT {
            partial.to_string()
        } else if paths::escapes(&dir) {
            return None;
        } else {
            format!("{dir}/{partial}")
        };
        Some(Climb { head, rest: segments.join("/") })
    }

    /// The capture segments that survive the climb, for a specifier whose
    /// substitution normalizes to `address`: `address` without the
    /// remainder, matched against the head. Empty when the climb consumes
    /// the whole capture; `None` when no capture reaches `address` that way.
    fn capture_head<'a>(&self, address: &'a str) -> Option<&'a str> {
        let kept = if self.rest.is_empty() {
            address
        } else if address == self.rest {
            paths::ROOT
        } else {
            address.strip_suffix(self.rest.as_str())?.strip_suffix('/')?
        };
        let head_dir = self.head.strip_suffix('/');
        if kept == paths::ROOT {
            return self.head.is_empty().then_some("");
        }
        if head_dir == Some(kept) {
            return Some("");
        }
        kept.strip_prefix(self.head.as_str())
    }
}

/// A specifier the inversion of a target pattern derives from a path, with
/// what derived it.
enum Derived<'p> {
    /// From a tsconfig's `paths`.
    Paths(&'p EffectiveConfig, String),
    /// From a workspace package's entries: the subpath (`"."` or `./x`) and
    /// the bare specifier naming it.
    Package(&'p WorkspacePackage, String, String),
    /// From a package.json `imports` map.
    Imports(&'p PackageImports, String),
}

/// Every distinct tsconfig of the project, and the specifiers derived from
/// a path by inverting each target pattern against it and its addresses.
struct Derivation<'p> {
    configs: Vec<&'p EffectiveConfig>,
    specifiers: Vec<Derived<'p>>,
}

/// The specifiers that may resolve to `path`: [`specifiers_targeting`] for
/// every tsconfig, [`subpaths_targeting`] for every workspace package that
/// may name `path` ([`package_may_name`]), [`imports_specifiers_targeting`]
/// for every `imports` map, each over `path` and its addresses
/// ([`addresses_resolving_to`]).
fn derive_specifiers<'p>(project: &'p TsProject, path: &str) -> Derivation<'p> {
    let mut addresses = addresses_resolving_to(path);
    addresses.push(path.to_string());
    let mut specifiers: Vec<Derived<'p>> = Vec::new();

    let configs = distinct_configs(project);
    for config in &configs {
        let derived: BTreeSet<String> =
            addresses.iter().flat_map(|address| specifiers_targeting(config, address)).collect();
        specifiers.extend(derived.into_iter().map(|specifier| Derived::Paths(config, specifier)));
    }

    for (name, package) in &project.packages {
        if !package_may_name(package, path) {
            continue;
        }
        let subpaths: BTreeSet<String> =
            addresses.iter().flat_map(|address| subpaths_targeting(package, address)).collect();
        for subpath in subpaths {
            let specifier = match subpath.strip_prefix('.') {
                Some(rest) if !rest.is_empty() => format!("{name}{rest}"),
                _ => name.clone(),
            };
            specifiers.push(Derived::Package(package, subpath, specifier));
        }
    }

    for config in project.imports_by_dir.values().flatten() {
        let derived: BTreeSet<String> =
            addresses.iter().flat_map(|address| imports_specifiers_targeting(config, address)).collect();
        specifiers.extend(derived.into_iter().map(|specifier| Derived::Imports(config, specifier)));
    }

    Derivation { configs, specifiers }
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

/// The address an unresolved non-relative `specifier` is recorded at: the
/// first base of the list [`resolve`] walks for it, so the create of any of
/// its candidates can name it ([`waiting_addresses_for`]). A `#` specifier
/// takes the first target of `imports`; a bare one naming a workspace
/// package the package's first entry, else the first `paths` expansion
/// under `config`. `None` for a relative specifier, one no family matches
/// (an npm package, a builtin), or one only the catch-all `"*"` entry
/// matches: that entry is a lookup before `node_modules`, so a package
/// import it matches stays external.
pub fn waiting_address(
    project: &TsProject,
    specifier: &str,
    config: Option<&EffectiveConfig>,
    imports: Option<&PackageImports>,
) -> Option<String> {
    if is_relative_specifier(specifier) {
        return None;
    }
    if specifier.starts_with('#') {
        return imports_targets(imports?, specifier).into_iter().next();
    }
    let workspace = parse_bare_specifier(specifier)
        .and_then(|(name, subpath)| Some(package_entry_targets(project.packages.get(&name)?, &subpath)))
        .and_then(|entries| entries.into_iter().next());
    workspace.or_else(|| {
        let config = config.filter(|config| matched_beyond_catch_all(config, specifier))?;
        expand_paths_candidates(config, specifier).into_iter().next()
    })
}

/// The `paths` pattern that matches every bare specifier.
const CATCH_ALL_PATTERN: &str = "*";

/// Whether an entry of `config` other than the catch-all `"*"` matches
/// `specifier`.
fn matched_beyond_catch_all(config: &EffectiveConfig, specifier: &str) -> bool {
    config.paths.iter().any(|entry| {
        entry.pattern != CATCH_ALL_PATTERN
            && if entry.pattern.contains('*') {
                match_wildcard(&entry.pattern, specifier).is_some()
            } else {
                entry.pattern == specifier
            }
    })
}

/// [`waiting_address`] for `specifier` written in `from`, under the tsconfig
/// and `imports` map nearest `from`. `None` for a relative specifier (its
/// address is [`relative_address`]) or an importer outside the root.
pub fn miss_address(project: &TsProject, specifier: &str, from: &RelPath) -> Option<String> {
    if is_relative_specifier(specifier) {
        return None;
    }
    let dir = importer_dir(from)?;
    waiting_address(project, specifier, project.tsconfig_for(dir), project.imports_for(dir))
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
