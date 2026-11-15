//! Names the front and the shim agree on: the tool a front adds, the `_meta`
//! key a selection carries and the argument that routes a call.

/// The tool a front adds to the usual eight.
pub const SELECT_PROJECT: &str = "select_project";

/// The `_meta` key a successful selection carries; the shim (slice 5) acts
/// on it and removes it before the client sees the result.
pub const SWITCH_PROJECT_META: &str = "g-mesh/switchProject";

/// The argument a front adds to the eight tools' schemas. The shim routes a
/// call that carries it to that project's daemon and removes it before
/// forwarding, so no daemon's own schema has it.
pub const PROJECT_ARG: &str = "project";
