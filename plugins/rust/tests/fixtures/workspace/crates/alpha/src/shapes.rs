//! Traits, impls and the two call shapes that separate what the structural
//! tier can answer from what it cannot.

use crate::internals::crate_only;

/// The trait `find_implementations` is checked on: `Square` and `Circle`
/// implement it, and both `SUPERTYPE_OF` edges are same-file and resolved.
pub trait Shape {
    /// A required method - every implementor declares its own.
    fn area(&self) -> u8;

    /// A default method, which `describe` reaches through `self`.
    fn label(&self) -> &'static str {
        "shape"
    }
}

/// Two traits declaring one method name, implemented on one type. Their two
/// impls are `<Circle as Loud>::speak` and `<Circle as Quiet>::speak`: with
/// the design doc's sketched `Circle::speak` they would be one id and one of
/// them would be missing from the index.
pub trait Loud {
    fn speak(&self) -> &'static str;
}

pub trait Quiet {
    fn speak(&self) -> &'static str;
}

pub struct Square {
    pub side: u8,
}

pub struct Circle;

impl Square {
    /// An associated function reached by a type-qualified path from the other
    /// crate - `Square::new(3)`, addressed by `qualifiedName`.
    pub fn new(side: u8) -> Self {
        Square { side }
    }
}

impl Shape for Square {
    fn area(&self) -> u8 {
        self.side * self.side
    }
}

impl Shape for Circle {
    /// Calls a `pub(crate)` item of a *sibling* module: allowed, because
    /// `alpha` is on this module's parent chain.
    fn area(&self) -> u8 {
        crate_only()
    }
}

impl Loud for Circle {
    fn speak(&self) -> &'static str {
        "LOUD"
    }
}

impl Quiet for Circle {
    fn speak(&self) -> &'static str {
        "quiet"
    }
}

/// A receiver call: `square.area()` gets **no** edge, only an open site, and
/// the semantic tier (GM-290) is what turns it into one.
pub fn total(square: &Square) -> u8 {
    square.area()
}

/// Trait dispatch through a generic - a documented structural gap. Which
/// `area` this reaches depends on the type argument at each call site, which
/// is exactly what a structural tier cannot know.
pub fn describe<S: Shape>(shape: &S) -> &'static str {
    shape.label()
}
