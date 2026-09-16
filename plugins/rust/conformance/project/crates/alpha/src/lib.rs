//! Fixture crate exercising every module-tree shape `plugins/rust`'s project
//! model has to resolve: a file-based `mod` (`nested`, itself a `mod.rs`
//! with its own further child), an inline module, and a `#[path]` module.
//! `orphan.rs`, in this same directory, is deliberately unreferenced.
//!
//! Since GM-286 it also exercises the *extractor*: `shapes` has the traits
//! and impls `find_implementations` is checked on, `internals` has the
//! `pub(crate)` item whose visibility is checked from a sibling module and
//! from the other crate, `prelude` is the `pub use` chain, and `gaps` is
//! where the documented structural gaps are written down beside code that
//! has them.

mod nested;

mod inline_mod {
    pub fn helper() -> &'static str {
        "inline"
    }
}

#[path = "other_impl.rs"]
mod imp;

pub mod gaps;
pub mod internals;
pub mod prelude;
pub mod shapes;

pub fn run() -> &'static str {
    let _ = inline_mod::helper();
    let _ = imp::detail();
    nested::deep::work()
}
