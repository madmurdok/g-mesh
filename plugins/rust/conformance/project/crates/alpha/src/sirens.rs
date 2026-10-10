//! The cross-crate implementation only rust-analyzer's sweep finds - the
//! fixture's witness that the semantic tier is measured, not described.
//!
//! `wail_for!` writes `impl Wail for $t` wherever it is invoked. The
//! structural tier does not expand macros, so `crates/beta/src/alarm.rs`'s
//! `alpha::wail_for!(Siren)` is no `impl` to it; rust-analyzer expands it, and
//! its implementation sweep over `Wail` answers with `Siren` in the other
//! crate.

pub trait Wail {
    fn wail(&self) -> u8;
}

#[macro_export]
macro_rules! wail_for {
    ($t:ident) => {
        impl $crate::sirens::Wail for $t {
            fn wail(&self) -> u8 {
                7
            }
        }
    };
}
