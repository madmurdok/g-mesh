//! `g-mesh-plugin-python`'s library half: the [`Extractor`](g_mesh_plugin_sdk::Extractor)
//! and the project model it runs on. Split from `main.rs` - which is a thin
//! shim calling [`run`](g_mesh_plugin_sdk::run) - for exactly the reason
//! `plugins/rust/src/lib.rs` gives for itself: a binary crate's `pub` items
//! that nothing in that binary happens to call are dead code as far as
//! `rustc`/`clippy` can tell, and a library crate's `pub` surface is not - so
//! the two halves of this plugin (`project`'s public API and `extractor`'s)
//! can be tested and documented as an API rather than as whatever `main`
//! happens to reach.
//!
//! See `project`'s module doc for the package model (container keys, roots,
//! namespace packages, `.pyi` stubs - the eight decisions GM-295 and GM-296
//! settled between them), `extractor`'s for the structural tier itself (what a
//! `qualifiedName` is, why every declaration is `public`, what each import
//! shape emits, and what stays a documented gap), and `semantic`'s for the
//! pyright tier over the SDK's `LspBridge` - including the three places it
//! looks for a server and the two things pyright does differently from
//! rust-analyzer.

pub mod extractor;
pub mod project;
pub mod semantic;
