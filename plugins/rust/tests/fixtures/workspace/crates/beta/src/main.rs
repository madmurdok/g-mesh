//! The other crate: everything here addresses `alpha` from outside it, which
//! is what makes the visibility half of the acceptance criteria checkable.

use alpha::prelude::exported_helper;
use alpha::prelude::*;
use alpha::shapes::{describe, Square};

// DELIBERATELY NOT COMPILABLE. `alpha::internals::crate_only` is
// `pub(crate)`, so `rustc` would reject this `use` - and that is the point:
// the plugin still emits the placeholder, and core's visibility check is what
// must refuse to link it. `alpha` is not on `beta_crate`'s parent chain, so
// `container(alpha)` does not reach here. Nothing ever builds this fixture;
// `g-mesh plugins check` only indexes it.
use alpha::internals::crate_only;

fn main() {
    let square = Square::new(3);
    let _ = describe(&square);
    let _ = exported_helper();
    println!("beta");
}

/// The other half of the `pub(crate)` case: a call that must produce a
/// placeholder and must *not* end up as a caller of `internals::crate_only`.
fn forbidden() -> u8 {
    crate_only()
}
