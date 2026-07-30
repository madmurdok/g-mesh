//! Garbage-collection support for `~/.g-mesh/projects/`: the bookkeeping that
//! lets a later scan tell which project indexes are still in use from which
//! have been idle for months.
//!
//! Nothing here ever deletes anything. Per the architecture doc, deletion is
//! always an explicit `g-mesh clean` (see `cli::clean`); this module only
//! maintains and reads the facts that decision is made from.

pub mod last_used;
