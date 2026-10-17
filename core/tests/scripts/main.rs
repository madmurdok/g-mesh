//! Core's integration tests of the files under `scripts/`: each runs a
//! script for real, or checks what it reads. Separate from `it` so that a
//! release or tooling task that edits only these recompiles this small crate
//! and not the product's tests (GM-623).
//!
//! Placement rule (GM-623): a core integration test whose subject is a file
//! under `scripts/` (it runs that script, or parses its output) goes to
//! `core/tests/scripts/`; every other one goes to `core/tests/it/`. Either way
//! it is a module here, so a new file needs its own `mod <file>;` line below,
//! and `it/layout.rs` fails, naming the file, on one that has none or on a new
//! top-level `core/tests/*.rs` (which cargo would build as a binary of its own).
//!
//! Run one file's tests:
//! `cargo nextest run -p g-mesh --test scripts -E 'test(/^<file>::/)'`.

mod ci_junit_summary_script;
mod cut_release_plugin_versions;
mod prune_stale_objects_script;
mod release_packaging_scripts;
mod test_local_script;
mod test_sections_scripts;
mod test_select_scripts;
