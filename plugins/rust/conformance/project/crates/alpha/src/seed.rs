//! `schema` is this module's named import; its inline child `inline` and its
//! file-backed child `tests` reach it only through `use super::*;`, so their
//! `schema::set_facts` calls are open sites the semantic tier answers.

use crate::store::schema;

pub fn direct() -> u32 {
    schema::set_facts(1)
}

pub mod inline {
    use super::*;

    pub fn via_glob_inline() -> u32 {
        schema::set_facts(2)
    }
}

#[cfg(test)]
mod tests;
