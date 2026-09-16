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

/// GM-290's cross-crate implementation case: a type of *this* crate
/// implementing a trait of the other one.
///
/// `Loud` is not imported by item here - it arrives through
/// `use alpha::prelude::*` at the top of this file - so the structural tier
/// emits nothing for the `impl` below: not an edge, and not even an open
/// site (see `crates/alpha/src/prelude.rs`'s own comment for the resolution
/// path). `find_implementations` on `shapes::Loud` therefore answers
/// `{Circle}` structurally and `{Circle, Megaphone}` after a semantic pass,
/// which is what makes `conformance/expect.toml`'s entry for it a real
/// measurement of the rust-analyzer tier rather than of the parser.
pub struct Megaphone;

impl Loud for Megaphone {
    fn speak(&self) -> &'static str {
        "BETA"
    }
}
