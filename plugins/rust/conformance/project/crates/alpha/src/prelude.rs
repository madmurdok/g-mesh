//! A `pub use` chain: this module declares nothing and publishes four
//! things, one of them under a new name.
//!
//! `Loud` is here for GM-290 and is the one name nothing imports *by item*:
//! `crates/beta/src/main.rs` reaches it through `use alpha::prelude::*`, a
//! glob. A bare *type* reached that way is still `Bound::Nothing`
//! (`extractor/bodies.rs`'s `resolve_bare`, the `want == Type` arm), but a
//! trait *clause* is not (GM-537, `Bodies::resolve_supertype`): it becomes a
//! `name` key in beta's root module, which core follows through the glob to
//! here and through the `pub use` below to `shapes::Loud`. So beta's
//! `impl Loud for Megaphone` and its `speak` are linked structurally.
//!
//! `crates/beta/src/main.rs` imports `exported_helper` from *here*, so the
//! only way its call can reach `internals::published` is core's re-export
//! walk - which is what makes this an acceptance case rather than a
//! convenience.

pub use crate::internals::published as exported_helper;
pub use crate::shapes::{Loud, Shape, Square};
