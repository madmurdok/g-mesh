//! Garbage-collection support for `~/.g-mesh/projects/`: the bookkeeping that
//! lets a later scan tell which project indexes are still in use from which
//! have been idle for months.
//!
//! Nothing in this module deletes anything: it maintains and reads the facts a
//! deletion is decided from. An explicit deletion is `g-mesh clean` (`cli::clean`);
//! the unattended one, of state whose project directory is gone, is the daemon's own
//! purge and the shim's startup sweep (`control::purge`, `control::sweep`).

pub mod last_used;
pub mod warning;
