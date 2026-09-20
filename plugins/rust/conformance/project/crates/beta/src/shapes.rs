//! GM-360, face A5: a module whose *path within its crate* collides with
//! `alpha`'s. `alpha::shapes` and `beta_crate::shapes` are different modules
//! in different crates, and a Rust qualifiedName carries neither crate name,
//! so both of their members are spelled `shapes::<name>` in the index -
//! ripgrep's `matcher::RegexMatcher`, which `crates/regex` and `crates/pcre2`
//! both declare, reduced to two files.

/// The namesake of `alpha::shapes::Gauge`.
pub struct Gauge;

/// A reference to *this* `Gauge`, so `[[references]]`' expected set over
/// `shapes::Gauge` differs by which crate's declaration was picked.
pub fn check(gauge: &Gauge) -> u8 {
    let _ = gauge;
    2
}
