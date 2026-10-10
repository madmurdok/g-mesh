//! `Siren` implements `alpha::sirens::Wail` only through a macro
//! of the other crate - see `crates/alpha/src/sirens.rs`.

pub struct Siren;

alpha::wail_for!(Siren);
