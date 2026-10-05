//! `g-mesh-plugin-typescript`: the TypeScript/JavaScript language plugin on
//! the Rust plugin SDK.
//!
//! The library half holds everything; `main.rs` only hands it to the SDK's
//! `run` loop, so tests drive the extractor in process.
//!
//! - [`extractor`]: one file's structural graph, from tree-sitter.
//! - [`project`]: the project model the extractor reads.

pub mod extractor;
pub mod project;
