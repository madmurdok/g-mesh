//! GM-361: the three `impl Trait for T` shapes whose `SUPERTYPE_OF` edge
//! does not start where the ordinary one does.
//!
//! `Direct` below is the ordinary shape - a type declared beside its impl -
//! and the three after it are the ones `find_implementations("Sink")`
//! answered wrongly on ripgrep: two blanket impls it left out entirely, and
//! one inside an inline module whose row came back as the *module*.

/// The trait `conformance/expect.toml`'s blanket `[[implementations]]` entry
/// is anchored on.
pub trait Sink {
    fn accept(&self) -> u8;
}

/// The ordinary shape, here so the entry's set is not made only of the odd
/// ones: its edge starts at this declaration.
pub struct Direct;

impl Sink for Direct {
    fn accept(&self) -> u8 {
        1
    }
}

/// A blanket impl whose self type is not a path at all, so no name can be
/// looked up and no declaration of this project can start the edge - the
/// `impl` block itself carries it, under the prefix its own methods already
/// have (`<&'a mut S as Sink>::accept`).
impl<'a, S: Sink> Sink for &'a mut S {
    fn accept(&self) -> u8 {
        (**self).accept()
    }
}

/// A blanket impl over a foreign generic type: `Box` is a path, but one this
/// file neither declares nor imports, which is exactly what "names nothing
/// this project declares" means.
impl<S: Sink + ?Sized> Sink for Box<S> {
    fn accept(&self) -> u8 {
        (**self).accept()
    }
}

/// An inline module, so that an `impl` header sits inside a `Module` node
/// rather than at the file's top level. `Inner` is an ordinary implementor;
/// the assertion is that `blanket::inner` is **not** a second one.
pub mod inner {
    use super::Sink;

    pub struct Inner;

    impl Sink for Inner {
        fn accept(&self) -> u8 {
            2
        }
    }
}
