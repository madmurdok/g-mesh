//! A `pub use` chain: this module declares nothing and publishes four
//! things, one of them under a new name.
//!
//! `Loud` is here for GM-290 and is the one name nothing imports *by item*:
//! `crates/beta/src/main.rs` reaches it through `use alpha::prelude::*`, a
//! glob, which the structural tier resolves to `Bound::Nothing` - no edge and
//! not even an open site (`extractor/bodies.rs`'s `resolve_bare`, the
//! `want == Type` arm). So beta's `impl Loud for Megaphone` is invisible to
//! every amount of parsing, and the only thing that finds it is the semantic
//! tier's `textDocument/implementation` sweep over the trait itself.
//!
//! `crates/beta/src/main.rs` imports `exported_helper` from *here*, so the
//! only way its call can reach `internals::published` is core's re-export
//! walk - which is what makes this an acceptance case rather than a
//! convenience.

pub use crate::internals::published as exported_helper;
pub use crate::shapes::{Loud, Shape, Square};
