//! `g-mesh-plugin-python`'s library half: the [`Extractor`](g_mesh_plugin_sdk::Extractor)
//! and the project model it runs on. Split from `main.rs` - which is a thin
//! shim calling [`run`](g_mesh_plugin_sdk::run) - for exactly the reason
//! `plugins/rust/src/lib.rs` gives for itself: [`project::ProjectContext`]'s
//! public API (`load`, `roots`, `container_for`, `notes`) is this task's
//! (GM-295's) actual deliverable, and a binary crate's `pub` items that
//! nothing in that binary happens to call yet are dead code as far as
//! `rustc`/`clippy` can tell - a library crate's `pub` surface is not,
//! because GM-296's extractor is exactly the downstream user this task has
//! no way to write yet.
//!
//! See `project`'s module doc for the project model itself (container keys,
//! roots, namespace packages, `.pyi` stubs - the seven decisions this task
//! had to settle), and `extractor`'s for why `extract` is presently a
//! `File`-only stub.

pub mod extractor;
pub mod project;
