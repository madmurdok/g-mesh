//! Filesystem metadata helpers.

use std::fs;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};

/// Milliseconds since the Unix epoch, per `metadata.modified()`. Kept as a
/// plain integer (rather than e.g. an RFC3339 string) so the fast-path
/// comparison in `ensure_fresh` is a cheap integer equality check.
///
/// Shared with `cli::status`, which compares the same recorded baselines to
/// count how many files the index still owes work for - it has to read an
/// `indexed_files` row exactly the way the code that wrote it does.
pub(crate) fn mtime_millis(metadata: &fs::Metadata) -> Result<i64> {
    let modified = metadata.modified().context("filesystem does not report mtimes")?;
    let duration = modified.duration_since(UNIX_EPOCH).context("mtime is before the Unix epoch")?;
    Ok(duration.as_millis() as i64)
}
