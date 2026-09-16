//! A `pub use` chain: this module declares nothing and publishes three
//! things, one of them under a new name.
//!
//! `crates/beta/src/main.rs` imports `exported_helper` from *here*, so the
//! only way its call can reach `internals::published` is core's re-export
//! walk - which is what makes this an acceptance case rather than a
//! convenience.

pub use crate::internals::published as exported_helper;
pub use crate::shapes::{Shape, Square};
