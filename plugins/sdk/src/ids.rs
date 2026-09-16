//! Node and edge ids: the one thing in this crate that is a contract with
//! plugins written in other languages, not just with core.
//!
//! # Why the scheme is fixed rather than a plugin's own business
//!
//! An id is what core stores a row under, what a diff deletes by, and what an
//! edge points at. Nothing *forces* two plugins to spell ids the same way -
//! they own disjoint files, so a collision between languages is not the
//! worry. What forces it is that the scheme is the same problem three times,
//! and three independent answers would differ in the details that matter
//! (what separates the fields, whether an absent field is an empty one, how
//! much of the digest is kept) exactly where a mistake is invisible: a plugin
//! whose bulk path and incremental path disagree by one byte never deletes
//! the rows it replaces, so every edited file silently accumulates
//! duplicates. That is the failure `id-stability.incremental-matches-bulk`
//! exists to catch, and the cheapest way not to have it in a fourth language
//! is for the fourth language not to invent a fourth answer.
//!
//! **`plugins/go` must match this**, and so must every later plugin. The
//! specification below is the whole contract; it is deliberately written as
//! bytes rather than as code, so a Go or C# implementation can be checked
//! against it without reading Rust.
//!
//! # Specification
//!
//! Both ids are the **lowercase hex SHA-256 digest of a byte string,
//! truncated to its first 32 characters** (16 bytes of digest). The byte
//! string is a list of fields joined by a single NUL byte (`0x00`), with a
//! literal tag as the first field, encoded UTF-8.
//!
//! **Node id** - five fields, always all five:
//!
//! ```text
//! "node" NUL <filePath> NUL <kind> NUL <qualifiedName> NUL <nativeKind>
//! ```
//!
//! - `<filePath>`: project-relative, forward slashes - the same string the
//!   node's own `filePath` carries.
//! - `<kind>`: the wire spelling of [`NodeKind`] - `File`, `Module`, `Type`,
//!   `Function`, `Variable`.
//! - `<qualifiedName>`: the node's own `qualifiedName`.
//! - `<nativeKind>`: the node's own `nativeKind`, or **the empty string**
//!   when it has none. Empty, not omitted: the field is always present, so
//!   the separator count never varies.
//!
//! **Edge id** - four fields, plus a fifth only when the edge binds a
//! particular declaration of its target:
//!
//! ```text
//! "edge" NUL <fromId> NUL <kind> NUL <toId> [ NUL <toDeclaration> ]
//! ```
//!
//! - `<kind>`: the wire spelling of [`EdgeKind`] - `DEFINES`, `IMPORTS`,
//!   `CALLS`, `SUPERTYPE_OF`, `REFERENCES`, `EXPORTS`.
//! - `<toDeclaration>`: the ordinal in decimal, **and the whole field
//!   including its separator is absent** when there is none - unlike
//!   `nativeKind` above. That asymmetry is not a slip: every edge the
//!   structural tier emits has no ordinal, and appending a trailing separator
//!   to all of them would have changed every edge id in every index in the
//!   field for nothing. An ordinal of `0` is a binding like any other and
//!   produces the field.
//!
//! # Why an id is derived from identity and never from position
//!
//! Ids key the incremental diff. A function whose body grew keeps its id and
//! changes its range, so the diff reports it as one changed symbol - deleted
//! and re-upserted under the same id, which is how its inbound edges survive
//! the edit. If ids carried positions, an insertion anywhere above a symbol
//! would delete it and add a stranger, and every edge into it would dangle.
//! `nativeKind` is in the node id for the one case where identity is not
//! enough on its own: a getter and a setter can share a `qualifiedName`.
//!
//! # Equivalence with the TS plugin
//!
//! `tests/id_scheme.rs` asserts this implementation against ids computed by
//! `plugins/typescript`'s own `nodeIdFor`/`edgeIdFor`
//! (`plugins/typescript/src/extract.ts`), recorded as literal expected
//! values. They are recorded rather than recomputed at test time because the
//! point is to pin *this* crate against what the other implementation
//! actually produced, not to re-run the other implementation and agree with
//! whatever it does today.

use sha2::{Digest, Sha256};

use g_mesh_wire::{EdgeKind, NodeKind};

/// The separator between fields of a hashed id. A NUL byte, so no field's
/// own content can ever imitate it: a file path may contain a space, a
/// qualified name may contain `::` or `#`, and neither may contain `0x00`.
const FIELD_SEPARATOR: u8 = 0;

/// How many hex characters of the digest an id keeps.
///
/// 32 characters is 128 bits, which is not a security claim but a collision
/// one: an index of a million nodes sits around `10^-27` chance of any pair
/// colliding. The remaining 32 characters buy nothing and cost a wider column
/// in every row and every edge endpoint.
const ID_HEX_LEN: usize = 32;

/// The digest half of the scheme: hex SHA-256 of `fields` joined by NUL, cut
/// to [`ID_HEX_LEN`].
fn digest(tag: &str, fields: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(tag.as_bytes());
    for field in fields {
        hasher.update([FIELD_SEPARATOR]);
        hasher.update(field.as_bytes());
    }
    let mut id = format!("{:x}", hasher.finalize());
    id.truncate(ID_HEX_LEN);
    id
}

/// The wire spelling of a [`NodeKind`], as it appears in a node's `kind`
/// field and therefore in its id.
///
/// Round-tripped through serde rather than written out by hand, so this stays
/// correct if the enum ever gains a rename attribute - the same reasoning
/// `watcher::apply::edge_kind_wire_value` gives in core. The fallback can
/// only be reached if serialization itself fails, which for a fieldless enum
/// it cannot.
pub fn node_kind_wire_value(kind: NodeKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{kind:?}"))
}

/// The wire spelling of an [`EdgeKind`] (`CALLS`, `SUPERTYPE_OF`, ...) - see
/// [`node_kind_wire_value`].
pub fn edge_kind_wire_value(kind: EdgeKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{kind:?}"))
}

/// A node's id, per this module's specification.
///
/// `native_kind` is `None` for an ordinary declaration and `Some` for
/// anything whose `nativeKind` distinguishes it from another node with the
/// same qualified name - a placeholder, a getter beside its setter. `None`
/// hashes as the empty string, which is the same thing the TS plugin's
/// `nativeKind ?? ""` does.
pub fn node_id(file_path: &str, kind: NodeKind, qualified_name: &str, native_kind: Option<&str>) -> String {
    digest("node", &[file_path, &node_kind_wire_value(kind), qualified_name, native_kind.unwrap_or("")])
}

/// An edge's id, per this module's specification.
///
/// `to_declaration` is `Some` only on a `CALLS` edge a semantic tier has
/// bound to one particular declaration of an overloaded target. Absent, it
/// contributes no field *and no separator* - see the module doc for why this
/// one field is asymmetric with `nativeKind`.
pub fn edge_id(from_id: &str, kind: EdgeKind, to_id: &str, to_declaration: Option<u32>) -> String {
    let kind = edge_kind_wire_value(kind);
    match to_declaration {
        None => digest("edge", &[from_id, &kind, to_id]),
        Some(ordinal) => digest("edge", &[from_id, &kind, to_id, &ordinal.to_string()]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_is_thirty_two_lowercase_hex_characters() {
        let id = node_id("src/a.rs", NodeKind::Function, "foo", None);
        assert_eq!(id.len(), 32, "{id}");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()), "{id}");
    }

    #[test]
    fn kinds_hash_as_their_wire_spelling() {
        assert_eq!(node_kind_wire_value(NodeKind::Function), "Function");
        assert_eq!(node_kind_wire_value(NodeKind::File), "File");
        assert_eq!(edge_kind_wire_value(EdgeKind::SupertypeOf), "SUPERTYPE_OF");
        assert_eq!(edge_kind_wire_value(EdgeKind::Calls), "CALLS");
    }

    /// The point of a NUL separator: no field's content can imitate it, so two
    /// different field splits cannot produce the same byte string. With a
    /// space (or any printable character) these two would collide.
    #[test]
    fn the_separator_cannot_be_forged_by_a_fields_own_content() {
        let shifted = node_id("a", NodeKind::Function, "b", Some("c"));
        let packed = node_id("a\u{0}Function\u{0}b", NodeKind::Function, "", Some("c"));
        assert_ne!(shifted, packed);
    }

    /// An absent `nativeKind` is the empty *field*, not a missing one - so it
    /// is the same id an explicit `Some("")` gives, and a different one from
    /// any non-empty kind.
    #[test]
    fn an_absent_native_kind_hashes_as_an_empty_field() {
        assert_eq!(
            node_id("a.rs", NodeKind::Type, "T", None),
            node_id("a.rs", NodeKind::Type, "T", Some(""))
        );
        assert_ne!(
            node_id("a.rs", NodeKind::Type, "T", None),
            node_id("a.rs", NodeKind::Type, "T", Some("x"))
        );
    }

    /// An absent `toDeclaration` contributes no field at all, so an unbound
    /// edge is not the same as one bound to nothing - and ordinal 0 is a
    /// binding, not an absence.
    #[test]
    fn an_absent_to_declaration_contributes_no_field_and_zero_is_a_binding() {
        let unbound = edge_id("f", EdgeKind::Calls, "t", None);
        let bound_zero = edge_id("f", EdgeKind::Calls, "t", Some(0));
        let bound_two = edge_id("f", EdgeKind::Calls, "t", Some(2));
        assert_ne!(unbound, bound_zero);
        assert_ne!(bound_zero, bound_two);
    }

    #[test]
    fn every_field_participates() {
        let base = node_id("a.rs", NodeKind::Function, "foo", Some("fn"));
        assert_ne!(base, node_id("b.rs", NodeKind::Function, "foo", Some("fn")));
        assert_ne!(base, node_id("a.rs", NodeKind::Variable, "foo", Some("fn")));
        assert_ne!(base, node_id("a.rs", NodeKind::Function, "bar", Some("fn")));
        assert_ne!(base, node_id("a.rs", NodeKind::Function, "foo", Some("method")));

        let edge = edge_id("f", EdgeKind::Calls, "t", None);
        assert_ne!(edge, edge_id("g", EdgeKind::Calls, "t", None));
        assert_ne!(edge, edge_id("f", EdgeKind::References, "t", None));
        assert_ne!(edge, edge_id("f", EdgeKind::Calls, "u", None));
    }
}
