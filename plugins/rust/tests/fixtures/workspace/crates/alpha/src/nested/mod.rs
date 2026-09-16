//! A directory module (`mod.rs`) with a further file-based child of its own,
//! a glob import, and a `pub use`.
//!
//! The two `use` items here are what `conformance/expect.toml` asserts
//! `get_dependencies` on: a glob and a named re-export are both imports of
//! the container they read from, and both link. This file is the one the
//! expectation anchors on because the kit's own session never edits it -
//! emptying a file GCs the containers whose only members were in it, and
//! takes every `IMPORTS` edge pointing at them with it.

use crate::internals::*;

mod deep;

pub use deep::work;
