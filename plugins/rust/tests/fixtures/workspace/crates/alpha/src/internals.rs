//! The crate's private half, for the `pub(crate)` acceptance case.
//!
//! [`crate_only`] is visible to `alpha` and every module under it - which is
//! what `container(alpha)` means to core's linker - and to nothing outside
//! the crate. `shapes` (a *sibling* module, not an ancestor) calls it and
//! must link; `crates/beta/src/main.rs` calls it and must not.

/// Visible throughout `alpha`, and nowhere else.
pub(crate) fn crate_only() -> u8 {
    1
}

/// Published, and re-exported again by `crate::prelude` under another name.
pub fn published() -> u8 {
    crate_only() + 1
}
