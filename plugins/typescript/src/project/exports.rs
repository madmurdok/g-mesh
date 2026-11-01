//! package.json `exports` and `imports` maps: which paths a subpath or a
//! `#name` could name, most specific first.
//!
//! Every matching key contributes, not only the most specific one, and the
//! existence set picks among the candidates: a map that still points at a
//! renamed file resolves through whichever key names a file that exists.

use crate::project::jsonc::Json;
use crate::project::paths;
use crate::project::resolve::names_file;

/// Conditions in the order their targets are tried: source and ESM before
/// CJS, `types` last. Unlisted conditions rank after all of these, in
/// declaration order.
pub const CONDITION_PRIORITY: [&str; 11] = [
    "source",
    "import",
    "module",
    "require",
    "default",
    "node",
    "browser",
    "development",
    "production",
    "types",
    "typings",
];

/// One package.json's `imports` map and the directory its targets resolve
/// against.
#[derive(Debug, Clone, PartialEq)]
pub struct PackageImports {
    /// Project-relative directory of the declaring package.json.
    pub dir: String,
    /// The map, in declaration order.
    pub imports: Vec<(String, Json)>,
}

/// The targets `exports` declares for `subpath` (`"."` for the package
/// itself, otherwise `./x`), unresolved, most specific first.
///
/// A string or an array is the package root's entry. An object is a subpath
/// map when any key is `.` or starts with `./`, and a condition map for the
/// root otherwise. An exact subpath key wins outright; without one every
/// wildcard key that matches contributes.
pub fn exports_targets(exports: Option<&Json>, subpath: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let Some(exports) = exports else {
        return targets;
    };
    let entries = match exports {
        Json::String(_) | Json::Array(_) => {
            if subpath == "." {
                collect_condition_targets(exports, &mut targets);
            }
            return targets;
        }
        Json::Object(entries) => entries,
        _ => return targets,
    };

    if !entries.iter().any(|(key, _)| key == "." || key.starts_with("./")) {
        if subpath == "." {
            collect_condition_targets(exports, &mut targets);
        }
        return targets;
    }

    if let Some(exact) = exports.get(subpath) {
        return accepted_key_targets(TargetMap::Exports, exact, None);
    }

    for (key, value) in entries {
        if let Some(capture) = match_wildcard(key, subpath) {
            targets.extend(accepted_key_targets(TargetMap::Exports, value, Some(capture)));
        }
    }
    targets
}

/// Every project-relative path the `#` specifier could name under `config`,
/// in declaration order ([`imports_base`] per target tsc accepts,
/// [`target_accepted`]).
pub fn imports_targets(config: &PackageImports, specifier: &str) -> Vec<String> {
    let mut targets = Vec::new();
    for (key, value) in &config.imports {
        let capture = if key.contains('*') {
            match match_wildcard(key, specifier) {
                Some(capture) => Some(capture),
                None => continue,
            }
        } else if key == specifier {
            None
        } else {
            continue;
        };
        targets.extend(accepted_key_targets(TargetMap::Imports, value, capture));
    }

    let mut resolved: Vec<String> = Vec::new();
    for target in targets {
        if let Some(inside) = imports_base(&config.dir, &target) {
            if !resolved.contains(&inside) {
                resolved.push(inside);
            }
        }
    }
    resolved
}

/// One `imports` target resolved against the package directory `dir`. As in
/// tsc, a target is a module name when its own spelling, before `\\` is
/// read as a separator, starts with neither `./` nor `../` (`.`, `..`, `.\\`,
/// `..\\..`); only a module name may name the project root. A `../` target
/// never gets here: tsc rejects it ([`target_accepted`]).
pub fn imports_target_inside(dir: &str, target: &str) -> Option<String> {
    if !target.starts_with("./") && !target.starts_with("../") {
        paths::inside_or_root(dir, target)
    } else {
        paths::inside(dir, target)
    }
}

/// One substituted `imports` target as a base the forward lookup probes.
/// Targets leaving the project are dropped, and so is one naming the project
/// root unless it is spelled as a module name ([`imports_target_inside`]),
/// the only form tsc resolves to the root's `index` file. A relative target
/// (`./`, `../`) must name a file as written ([`names_file`]): tsc tries no
/// extension or `index` file for it, so `./lib` and `./src/` name nothing.
pub fn imports_base(dir: &str, target: &str) -> Option<String> {
    let inside = imports_target_inside(dir, target)?;
    let relative = target.starts_with("./") || target.starts_with("../");
    (!relative || names_file(&inside)).then_some(inside)
}

/// The `#` specifiers whose [`imports_targets`] under `config` may include
/// `address`, in declaration order without repeats ([`keys_targeting`]).
pub fn imports_specifiers_targeting(config: &PackageImports, address: &str) -> Vec<String> {
    keys_targeting(TargetMap::Imports, &config.imports, &config.dir, address)
}

/// The subpaths whose [`exports_targets`] may include `address`, a path
/// under the project-relative package directory `dir`, in declaration order
/// without repeats ([`keys_targeting`]). Only a subpath map has keys to
/// invert; a string, array or condition map only declares the root `.`,
/// which the caller asks about anyway.
pub fn exports_subpaths_targeting(exports: Option<&Json>, dir: &str, address: &str) -> Vec<String> {
    match exports {
        Some(Json::Object(entries)) if entries.iter().any(|(key, _)| key == "." || key.starts_with("./")) => {
            keys_targeting(TargetMap::Exports, entries, dir, address)
        }
        _ => Vec::new(),
    }
}

/// The inverse of substituting a key's targets: for each key of `entries`
/// (an `exports` subpath map or an `imports` map, as `map` says) and each
/// target pattern of its value tsc accepts ([`accepted_key_targets`]
/// without a capture, conditions flattened) resolved against `dir`, a
/// wildcard key with the capture the pattern takes from `address` (`./*` ->
/// `./src/*.ts`, `src/u.ts` -> `./u`), or an exact key whose target is
/// `address`. A wildcard key whose target has no `*` gives none, and so does
/// a capture tsc rejects (a `node_modules` segment). Targets resolve the way
/// the forward lookup resolves them.
fn keys_targeting(map: TargetMap, entries: &[(String, Json)], dir: &str, address: &str) -> Vec<String> {
    let inside = match map {
        TargetMap::Exports => paths::inside,
        TargetMap::Imports => imports_target_inside,
    };
    let mut keys: Vec<String> = Vec::new();
    for (key, value) in entries {
        for target in accepted_key_targets(map, value, None) {
            let Some(pattern) = inside(dir, &target) else {
                continue;
            };
            let found = if key.contains('*') {
                match match_wildcard(&pattern, address) {
                    Some(capture) if target_accepted(map, &target, Some(capture)) => {
                        key.replacen('*', capture, 1)
                    }
                    _ => continue,
                }
            } else if pattern == address {
                key.clone()
            } else {
                continue;
            };
            if !keys.contains(&found) {
                keys.push(found);
            }
        }
    }
    keys
}

/// One key's value (a string, an array, or a condition object) as ranked
/// target strings, with the wildcard capture substituted for every `*`.
/// `exports` subpath keys and `imports` keys share this shape.
pub fn key_targets(value: &Json, capture: Option<&str>) -> Vec<String> {
    let mut patterns = Vec::new();
    collect_condition_targets(value, &mut patterns);
    match capture {
        Some(capture) => patterns.into_iter().map(|pattern| pattern.replace('*', capture)).collect(),
        None => patterns,
    }
}

/// Which map a target comes from. tsc validates `exports` and `imports`
/// targets alike, except that an `imports` target spelled as a module name
/// is looked up instead of rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetMap {
    Exports,
    Imports,
}

/// [`key_targets`], keeping only the target patterns tsc accepts with
/// `capture` ([`target_accepted`]).
pub fn accepted_key_targets(map: TargetMap, value: &Json, capture: Option<&str>) -> Vec<String> {
    let mut patterns = Vec::new();
    collect_condition_targets(value, &mut patterns);
    patterns
        .into_iter()
        .filter(|pattern| target_accepted(map, pattern, capture))
        .map(|pattern| match capture {
            Some(capture) => pattern.replace('*', capture),
            None => pattern,
        })
        .collect()
}

/// Whether tsc 5.9 accepts the `exports`/`imports` target pattern `target`
/// with the wildcard `capture` (`None` checks the pattern alone). A target
/// must start with `./` and have no `.`, `..` or `node_modules` segment after
/// that first one, nor may the capture have one (tsc: "invalid type for
/// target"), with `\\` read as a separator for the segments. The one
/// exception is an `imports` target spelled as a module name (not `./`,
/// `../` or rooted): tsc looks it up as written and checks no segment.
pub fn target_accepted(map: TargetMap, target: &str, capture: Option<&str>) -> bool {
    let Some(rest) = target.strip_prefix("./") else {
        return map == TargetMap::Imports && !target.starts_with("../") && !paths::is_rooted(target);
    };
    !has_invalid_target_segment(&paths::with_forward_slashes(rest))
        && capture.is_none_or(|capture| !has_invalid_target_segment(&paths::with_forward_slashes(capture)))
}

/// Whether a `/`-separated text has a `.`, `..` or `node_modules` segment
/// (a trailing `/` ignored).
pub fn has_invalid_target_segment(text: &str) -> bool {
    let text = text.strip_suffix('/').unwrap_or(text);
    text.split('/').any(|segment| matches!(segment, "." | ".." | "node_modules"))
}

/// Every string under `value`, conditions visited in [`condition_rank`]
/// order (stable: unranked conditions keep their declaration order). `null`
/// and other non-string leaves contribute nothing.
pub fn collect_condition_targets(value: &Json, out: &mut Vec<String>) {
    match value {
        Json::String(target) => out.push(target.clone()),
        Json::Array(items) => {
            for item in items {
                collect_condition_targets(item, out);
            }
        }
        Json::Object(entries) => {
            let mut ranked: Vec<&(String, Json)> = entries.iter().collect();
            ranked.sort_by_key(|(condition, _)| condition_rank(condition));
            for (_, nested) in ranked {
                collect_condition_targets(nested, out);
            }
        }
        _ => {}
    }
}

/// `condition`'s place in [`CONDITION_PRIORITY`]; unknown ones rank last.
pub fn condition_rank(condition: &str) -> usize {
    CONDITION_PRIORITY.iter().position(|known| *known == condition).unwrap_or(CONDITION_PRIORITY.len())
}

/// What the first `*` of `pattern` stands for when `subpath` matches it, or
/// `None` (also for a pattern with no `*`). Text after the first `*` is
/// literal.
pub fn match_wildcard<'a>(pattern: &str, subpath: &'a str) -> Option<&'a str> {
    let (prefix, suffix) = pattern.split_once('*')?;
    if subpath.len() < prefix.len() + suffix.len() {
        return None;
    }
    if !subpath.starts_with(prefix) || !subpath.ends_with(suffix) {
        return None;
    }
    Some(&subpath[prefix.len()..subpath.len() - suffix.len()])
}
