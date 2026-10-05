//! package.json `exports` and `imports` maps: which paths a subpath or a
//! `#name` could name, most specific first.
//!
//! Every matching key contributes, not only the most specific one, and the
//! existence set picks among the candidates: a map that still points at a
//! renamed file resolves through whichever key names a file that exists.

use crate::project::jsonc::Json;
use crate::project::paths;

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
        collect_condition_targets(exact, &mut targets);
        return targets;
    }

    for (key, value) in entries {
        if let Some(capture) = match_wildcard(key, subpath) {
            targets.extend(key_targets(value, Some(capture)));
        }
    }
    targets
}

/// Every project-relative path the `#` specifier could name under `config`,
/// in declaration order. Targets leaving the project are dropped.
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
        targets.extend(key_targets(value, capture));
    }

    let mut resolved: Vec<String> = Vec::new();
    for target in targets {
        if let Some(inside) = paths::inside(&config.dir, &target) {
            if !resolved.contains(&inside) {
                resolved.push(inside);
            }
        }
    }
    resolved
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
