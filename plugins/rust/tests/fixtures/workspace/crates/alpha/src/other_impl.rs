//! The file `#[path = "other_impl.rs"] mod imp;` (in `lib.rs`) points at -
//! its own name does not match the module name it backs (`imp`), which is
//! the whole point of the fixture.

pub fn detail() -> &'static str {
    "other-impl"
}
