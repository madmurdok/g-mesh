//! A module spelled like a type. Rust keeps modules and types in one
//! namespace, so `Keys` here can only be this module; the structural tier
//! reads it as one through the project's module registry rather than by its
//! capital letter. Called from `crates/beta/src/capital_calls.rs`.

#[allow(non_snake_case)]
pub mod Keys {
    pub enum Key {
        Major(u8),
    }

    pub fn f() {}
}
