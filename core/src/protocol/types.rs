//! The plugin wire protocol, re-exported.
//!
//! These types lived here in full until GM-284, when `plugins/sdk` became the
//! second Rust crate that has to speak this protocol. They now live in the
//! `g-mesh-wire` crate - whose module doc has the full reasoning - and this
//! module re-exports them unchanged, so every `use crate::protocol::types::…`
//! in core keeps naming exactly what it always named.
//!
//! Nothing is filtered on the way through, and nothing should be: a type that
//! is on the wire is core's business by definition, and one that is not does
//! not belong in that crate. `pub use …::*` rather than an enumerated list is
//! what keeps this file from becoming a second place to remember to update
//! when the protocol gains a type.
//!
//! The types' own tests moved with them, and run under `cargo test` at the
//! workspace root like every other crate's.

pub use g_mesh_wire::*;
