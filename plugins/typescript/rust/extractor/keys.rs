//! How a node is addressed: `qualifiedName`, `qualifiedPath`, and the
//! placeholder addresses.
//!
//! A `qualifiedName` is the lexical path joined by separators: `.` for
//! namespace and static members, `#` for instance members (`Store#pick`,
//! `Store.drop`). A dotted namespace name (`namespace Outer.Inner`) stays one
//! segment. A `#private` member keeps its `#` in its name, so `C##priv` is
//! `C`, then `#` + `#priv`. Node and edge ids are the SDK's
//! (`g_mesh_plugin_sdk::ids`), which is the same scheme.

use g_mesh_plugin_sdk::wire::{PathSegment, PlaceholderTarget, TargetKey, TargetScope};

/// `nativeKind` of a placeholder for an import specifier that names nothing
/// in this project.
pub const EXTERNAL_MODULE_NATIVE_KIND: &str = "external_module";
/// `nativeKind` of a placeholder for an import specifier resolved to a file of
/// this project; its `qualifiedName` is that file's path.
pub const RESOLVED_MODULE_NATIVE_KIND: &str = "resolved_module";
/// `nativeKind` of a placeholder for a symbol imported from a file of this
/// project; its `qualifiedName` is [`pending_symbol_qualified_name`].
pub const PENDING_SYMBOL_NATIVE_KIND: &str = "pending_symbol";
/// `nativeKind` of a placeholder for a name this file publishes without
/// declaring it (`export { a } from "./y"`).
pub const REEXPORT_NATIVE_KIND: &str = "reexport";
/// The name `export * from "./y"` is recorded under at both ends.
pub const REEXPORT_ALL_NAME: &str = "*";

/// Every `nativeKind` that marks a node as standing in for something outside
/// this file. An edge onto one is `resolved: false`; onto anything else,
/// `resolved: true`.
pub const PLACEHOLDER_NATIVE_KINDS: [&str; 4] = [
    EXTERNAL_MODULE_NATIVE_KIND,
    RESOLVED_MODULE_NATIVE_KIND,
    PENDING_SYMBOL_NATIVE_KIND,
    REEXPORT_NATIVE_KIND,
];

/// Whether a node of this `nativeKind` is a placeholder.
pub fn is_placeholder_kind(native_kind: Option<&str>) -> bool {
    native_kind.is_some_and(|kind| PLACEHOLDER_NATIVE_KINDS.contains(&kind))
}

/// The separator written before a member's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberSeparator {
    /// Namespace members and static class members.
    Dot,
    /// Instance class members.
    Hash,
}

impl MemberSeparator {
    pub fn as_str(self) -> &'static str {
        match self {
            MemberSeparator::Dot => ".",
            MemberSeparator::Hash => "#",
        }
    }
}

/// `name` under `prefix`, or `name` alone at the root.
pub fn qualify(prefix: &str, name: &str, separator: MemberSeparator) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}{}{name}", separator.as_str())
    }
}

/// The `qualifiedName` a path spells: each `sep` and `name`, concatenated.
pub fn join_path(path: &[PathSegment]) -> String {
    path.iter().map(|segment| format!("{}{}", segment.sep.as_deref().unwrap_or(""), segment.name)).collect()
}

/// A declaration's path and the `qualifiedName` it joins to, built together
/// so the two cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Qualified {
    pub qualified_name: String,
    pub qualified_path: Vec<PathSegment>,
}

/// [`qualify`] over segments.
pub fn qualified_in(prefix: &[PathSegment], name: &str, separator: MemberSeparator) -> Qualified {
    let mut qualified_path = prefix.to_vec();
    qualified_path.push(PathSegment {
        sep: (!prefix.is_empty()).then(|| separator.as_str().to_string()),
        name: name.to_string(),
    });
    Qualified { qualified_name: join_path(&qualified_path), qualified_path }
}

/// Whether core accepts `path` for a node named `name`: non-empty, no empty
/// name, a non-empty `sep` on every segment but the first and none on the
/// first, no U+001F or NUL, and ending in `name`. A path that fails is not
/// sent; the node keeps its `qualifiedName`.
pub fn is_sendable_path(path: &[PathSegment], name: &str) -> bool {
    let Some(last) = path.last() else { return false };
    if last.name != name {
        return false;
    }
    path.iter().enumerate().all(|(index, segment)| {
        let sep = segment.sep.as_deref();
        let sep_ok = if index == 0 { sep.is_none() } else { sep.is_some_and(|sep| !sep.is_empty()) };
        let forbidden = |text: &str| text.contains(['\u{1f}', '\0']);
        !segment.name.is_empty() && sep_ok && !forbidden(&segment.name) && !sep.is_some_and(forbidden)
    })
}

/// The `qualifiedName` of a placeholder for `name` in `target_file`:
/// `<file>#<name>`. Core splits it on the last `#`.
pub fn pending_symbol_qualified_name(target_file: &str, name: &str) -> String {
    format!("{target_file}#{name}")
}

/// The target every placeholder of this plugin carries: file-scoped and
/// name-keyed.
pub fn file_target(target_file: &str, name: &str) -> PlaceholderTarget {
    PlaceholderTarget {
        scope: TargetScope::File(target_file.to_string()),
        key: TargetKey::Name(name.to_string()),
        from_container: None,
        key_path: None,
    }
}
