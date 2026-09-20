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
    /// An associated function reached two qualifiedName-addressed ways:
    /// `Square::new(3)` from the other crate (a type-qualified path call)
    /// and `unit`'s `Self::new(1)` below (the same addressing, spelled
    /// `Self::` from inside the impl it names).
    pub fn new(side: u8) -> Self {
        Square { side }
    }

    /// A `Self::` call - resolved the same way `self.m()` is (`self_member`
    /// looks up the impl's own type first), but naming an associated
    /// function rather than calling through a receiver.
    pub fn unit() -> Self {
        Self::new(1)
    }

    /// Called two resolving ways below - `scale`'s `self.perimeter()` and
    /// `total_perimeter_via_path`'s `Square::perimeter(square)` - and one
    /// gapped way, `total_perimeter`'s `square.perimeter()`. See that
    /// function's own doc comment for what the three-way split is for.
    pub fn perimeter(&self) -> u8 {
        self.side * 4
    }

    /// `self.m()` inside an impl *is* resolved (README's "Two smaller ones,
    /// for completeness"), unlike a receiver call on a local variable - the
    /// second of `perimeter`'s three call sites.
    pub fn scale(&self) -> u8 {
        self.perimeter() * 2
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

/// The receiver-call gap, made airtight rather than merely asserted: this
/// function's `square.perimeter()` is a receiver call on a local variable,
/// exactly like `total`'s `square.area()` above, and gets no edge either.
/// `Square::scale` and `total_perimeter_via_path` below reach the *same*
/// `perimeter` declaration through the two call shapes this tier does
/// resolve - so `conformance/expect.toml`'s `[[callers]]` entry for
/// `shapes::Square::perimeter` can list exactly those two and omit this
/// function, which is what turns "the receiver call is missing" into a real
/// assertion: if a receiver call ever started resolving, this function
/// would show up in that entry's actual set and the check would fail on the
/// unexpected extra, not pass by never having been asked.
pub fn total_perimeter(square: &Square) -> u8 {
    square.perimeter()
}

/// The other resolving half of `total_perimeter`'s pair: a type-qualified
/// path call (`Square::perimeter(square)`, no receiver dot) addresses
/// `Square::perimeter` by its own `qualifiedName` tail, the same addressing
/// `keys.rs` documents for every `T::member` call - and, unlike a trait
/// impl's `<T as Tr>::m`, an inherent method's tail *is* `T::m`, so this one
/// resolves where the design doc's documented `Point::fmt()` gap would not.
pub fn total_perimeter_via_path(square: &Square) -> u8 {
    Square::perimeter(square)
}

/// Trait dispatch through a generic - a documented structural gap. Which
/// `area` this reaches depends on the type argument at each call site, which
/// is exactly what a structural tier cannot know.
pub fn describe<S: Shape>(shape: &S) -> &'static str {
    shape.label()
}

/// Dispatch through a **trait object**, the other half of GM-290's
/// receiver-call acceptance criterion and the one that separates two answers
/// a careless semantic tier would merge.
///
/// `total` above calls `area` through a variable whose type is `Square`, and
/// the honest answer there is `<Square as Shape>::area` - the impl's own
/// method. Here the receiver is `&dyn Shape`, and which `area` runs is a
/// run-time question, so the honest answer is `Shape::area`, the trait's
/// declaration. `conformance/expect.toml` asserts both as exact sets, which
/// means a pass that gave either call the other's answer fails twice over.
pub fn total_dyn(shape: &dyn Shape) -> u8 {
    shape.area()
}

/// GM-360, face A1: a type whose namesake sits at the *other* crate's root.
///
/// `crates/beta/src/main.rs` declares a `Ruler` too, and because a Rust
/// qualifiedName is a module path within its crate, beta's carries the bare
/// string `Ruler` while this one carries `shapes::Ruler`. Until GM-360 that
/// made the bare query `RegexMatcher`-shaped: exactly one declaration's
/// qualifiedName *was* the query, so `find_definition` resolved to it with
/// `resolvedBy: qualifiedName` and no ambiguity flag - an answer decided by
/// where a declaration happens to sit. `conformance/expect.toml`'s
/// `[[definition]] symbol = "Ruler"` is that case as a check.
pub struct Ruler;

/// GM-360, face A5: a type whose *qualifiedName* is not unique either.
///
/// `crates/beta/src/shapes.rs` is a `shapes` module too, and declares its own
/// `Gauge`, so `shapes::Gauge` names two declarations in this workspace -
/// exactly ripgrep's `matcher::RegexMatcher`, which `crates/regex` and
/// `crates/pcre2` both carry. Until GM-360 an exact qualifiedName match that
/// hit two rows was treated as no match at all and fell through to a bare-name
/// lookup no qualified spelling can ever match.
pub struct Gauge;

/// The one reference to *this* crate's `Gauge`, so the expectation over it is
/// an exact set that a resolution landing on beta's `Gauge` fails.
pub fn calibrate(gauge: &Gauge) -> u8 {
    let _ = gauge;
    1
}
