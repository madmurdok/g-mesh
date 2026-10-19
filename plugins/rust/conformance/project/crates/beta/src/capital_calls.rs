//! Calls into `alpha::capitals::Keys`, a module whose name starts
//! with a capital, from another crate - through a named `use` and through
//! the full path.

use alpha::capitals::Keys;

/// `Keys::f()` through the `use`: the qualifier is the module.
pub fn imported() {
    Keys::f();
}

/// The same call written in full, with no `use` involved.
pub fn full_path() {
    alpha::capitals::Keys::f();
}

/// A longer path whose first segment is the imported module.
pub fn variant() {
    let _ = Keys::Key::Major(4);
}
