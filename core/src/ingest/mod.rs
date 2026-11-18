//! Applies a plugin's answers to the index: the round trip, the wire->storage
//! mapping, the selection of what an edit affects, and query-time freshness.

pub mod apply;
pub(crate) mod convert;
pub(crate) mod select;
pub mod staleness;
