//! The `nativeKind` strings and names both sides of the plugin wire spell the
//! same way: what a plugin marks a placeholder node with, and what core
//! stores. One definition here, so core, the plugin SDK and the bundled
//! plugins cannot diverge; plugins in other languages mirror them and pin the
//! copy with a test that reads this file.

/// The `nativeKind` a plugin marks a pending cross-file symbol with. Mirrors
/// `PENDING_SYMBOL_NATIVE_KIND` in plugins/typescript/src/extractor/keys.rs - the
/// two are one wire contract and must be changed together.
pub const PENDING_SYMBOL_NATIVE_KIND: &str = "pending_symbol";

/// The `nativeKind` a plugin marks a re-export with: "this scope publishes
/// `name`, which really is its target". Mirrors `REEXPORT_NATIVE_KIND` in
/// plugins/typescript/src/extractor/keys.rs - the two are one wire contract and must be
/// changed together.
pub const REEXPORT_NATIVE_KIND: &str = "reexport";

/// The name a whole-module re-export (`export * from "./y"`) is recorded
/// under, as both its published name and its target key - it republishes
/// every name the target exports rather than one nameable one. Mirrors
/// `REEXPORT_ALL_NAME` in plugins/typescript/src/extractor/keys.rs.
pub const REEXPORT_ALL_NAME: &str = "*";

/// The `nativeKind` a plugin marks a resolved import placeholder with.
/// Mirrors `RESOLVED_MODULE_NATIVE_KIND` in plugins/typescript/src/extractor/keys.rs
/// - the two are one wire contract and must be changed together.
pub const RESOLVED_MODULE_NATIVE_KIND: &str = "resolved_module";

/// The `nativeKind` a plugin marks an import it could *not* resolve to
/// anything in this project with - a package, a language builtin, a crate
/// nothing here walks. Mirrors `EXTERNAL_MODULE_NATIVE_KIND` in
/// plugins/typescript/src/extractor/keys.rs and the same literal in the Python and
/// Rust plugins' emitters; all of them are one wire contract.
///
/// Unlike [`RESOLVED_MODULE_NATIVE_KIND`] and the two placeholder kinds above,
/// core never links one - there is nothing in the index for it to point at,
/// which is what "external" means here - so it is stored as an ordinary
/// `Module` row. It is exempt from the "a placeholder nativeKind requires a
/// target" rule in core's `protocol::conformance`.
pub const EXTERNAL_MODULE_NATIVE_KIND: &str = "external_module";

/// The `nativeKind` of a core-materialized container node. Never plugin-emitted
/// - `protocol::conformance` reports one on the wire as a violation.
pub const CONTAINER_NATIVE_KIND: &str = "container";
