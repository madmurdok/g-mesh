//! Node and edge kinds core itself names: the declaration filter shared by
//! every lookup and the linker, and the kinds of the container rows core writes.

use g_mesh_wire::native_kind::CONTAINER_NATIVE_KIND;
use g_mesh_wire::native_kind::{EXTERNAL_MODULE_NATIVE_KIND, RESOLVED_MODULE_NATIVE_KIND};
use g_mesh_wire::native_kind::{PENDING_SYMBOL_NATIVE_KIND, REEXPORT_NATIVE_KIND};

/// The `nativeKind`s that never answer a name, qualifiedName or position
/// lookup - see `graph::queries`' module header for what each one is and why it is here.
/// One list, because `graph::queries::find_by_name`, `graph::queries::find_by_qualified_name`,
/// `graph::queries::find_by_position` and `mcp::find_definition`'s candidate page all have
/// to agree about it.
pub(crate) const NON_DECLARATION_NATIVE_KINDS: [&str; 5] = [
    PENDING_SYMBOL_NATIVE_KIND,
    REEXPORT_NATIVE_KIND,
    RESOLVED_MODULE_NATIVE_KIND,
    EXTERNAL_MODULE_NATIVE_KIND,
    CONTAINER_NATIVE_KIND,
];

/// [`NON_DECLARATION_NATIVE_KINDS`] as a SQL condition on a `nodes` row.
/// `prefix` is the table alias and its dot (`"n."`), or `""` for an
/// unaliased `nodes`.
///
/// `nativeKind IS NULL OR ... NOT IN (...)` rather than the chain of `IS NOT`
/// this replaces: an ordinary declaration's `nativeKind` *is* NULL, and
/// `NULL NOT IN (...)` evaluates to NULL rather than true, so without the
/// explicit null arm the filter would exclude every real declaration. The
/// values interpolated are this crate's own `&'static str` constants and
/// never caller input, which is why they are written into the SQL rather than
/// bound - binding five of them at four call sites is what let the lists
/// drift in the first place.
pub(crate) fn declaration_only(prefix: &str) -> String {
    let kinds = NON_DECLARATION_NATIVE_KINDS.map(|kind| format!("'{kind}'")).join(", ");
    format!("({prefix}nativeKind IS NULL OR {prefix}nativeKind NOT IN ({kinds}))")
}

pub(crate) const MODULE_KIND: &str = "Module";
pub(crate) const DEFINES_KIND: &str = "DEFINES";
/// Only `graph::containers::defining_containers` needs this, to find the file node whose span
/// says which of a file's own declarations *is* the file.
pub(crate) const FILE_KIND: &str = "File";
