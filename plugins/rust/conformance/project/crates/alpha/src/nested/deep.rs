//! Two modules below the crate root, which is what makes this file the
//! `pub(crate)` parent-chain case.
//!
//! `alpha::nested` - this module's parent - declares nothing of its own: its
//! file holds a `use`, a `mod deep;` and a `pub use`, and of those only the
//! `mod` item is a container *member* (a placeholder never is). So
//! `alpha::nested` has a `containers` row, and therefore a `parentKey`, only
//! because `mod deep;` is emitted as a member of the module that declares it.
//! Drop that emission and `graph::containers::parent_chain` stops at
//! `alpha::nested`, `alpha` never appears on this module's chain, and
//! [`deep_work`]'s call below stops resolving.

use crate::internals::crate_only;

pub fn work() -> &'static str {
    "deep"
}

/// Reaches a `pub(crate)` item of the crate root from two modules down.
pub(crate) fn deep_work() -> u8 {
    crate_only()
}
