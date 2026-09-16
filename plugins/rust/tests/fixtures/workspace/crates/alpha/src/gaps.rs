//! The structural tier's documented gaps, written beside code that has them
//! so a reader can check each claim against a real declaration rather than
//! against prose. `plugins/rust/README.md` states them; this is where they
//! are exercised.

/// A `macro_rules!` **is** a node (`Function`, `nativeKind = "macro"`), so
/// `make_helper!(…)` below is a `CALLS` edge onto it.
macro_rules! make_helper {
    ($name:ident) => {
        pub fn $name() -> u8 {
            0
        }
    };
}

// GAP 1 - macro-generated items. `generated` is a real, public function of
// this module after expansion, and this plugin does not index it: nothing
// inside a macro body or a macro invocation's token tree is parsed. A call to
// `gaps::generated()` anywhere would stay unresolved.
make_helper!(generated);

// GAP 2 - `cfg` alternatives. Both of these are indexed, neither is chosen,
// and because they are the same Rust path in the same file they are *one*
// node: the first in source order, with the second merged into it. A caller
// may therefore see a callee defined under an inactive `cfg`, which is the
// gap the design doc already accepts.
#[cfg(unix)]
pub fn platform() -> &'static str {
    "unix"
}

#[cfg(windows)]
pub fn platform() -> &'static str {
    "windows"
}

// GAP 3 - trait dispatch through generics. `crate::shapes::describe` is the
// worked example; this is the same shape reached through the crate's own
// prelude, and its `shape.area()` is an open site with no edge.
pub fn measure<S: crate::prelude::Shape>(shape: &S) -> u8 {
    shape.area()
}
