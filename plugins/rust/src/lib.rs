//! `g-mesh-plugin-rust`'s library half: the [`Extractor`](g_mesh_plugin_sdk::Extractor)
//! and the project model it runs on. Split from `main.rs` - which is a thin
//! shim calling [`run`](g_mesh_plugin_sdk::run) - the same way `plugins/sdk`
//! itself separates its `[lib]` from the toy plugin's `[[bin]]`, and for the
//! same reason beyond symmetry: [`project::ProjectContext`]'s public API
//! (`crates`, `container_for`, `notes`) is this task's actual deliverable
//! per the task's own "what GM-286 inherits" framing, and a binary crate's
//! `pub` items that nothing in that binary happens to call yet are dead code
//! as far as `rustc`/`clippy` can tell - a library crate's `pub` surface is
//! not, because a downstream crate might use it, which is exactly GM-286's
//! relationship to this one. GM-286 replaces `extractor::RustExtractor::extract`'s
//! body in place; it does not need a second crate to depend on this one from.
//!
//! See `project`'s module doc for the project model itself, and
//! `extractor`'s for why `extract` is presently a stub.

pub mod extractor;
pub mod project;
pub mod semantic;
