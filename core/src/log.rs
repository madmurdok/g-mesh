//! Line-atomic stderr logging for daemon-process code.
//!
//! A shim-bootstrapped daemon's stderr is the project's `daemon.log`, opened
//! `O_APPEND` and shared with every plugin process it spawns (they inherit
//! it) and with any other daemon pointed at the same `G_MESH_DAEMON_LOG`.
//! A line from one writer survives intact only if it reaches the file in a
//! single `write(2)`. `eprintln!` does one write per format piece, so two
//! processes can split each other's lines; [`log_line!`](crate::log_line)
//! formats the whole line, newline included, and writes it once.
//!
//! `docs/architecture/gm-520-line-atomic-logs.md` is the design. The plugin
//! SDK carries an identical copy (`g_mesh_plugin_sdk::log_line!`), since the
//! SDK does not depend on core.

/// Writes one line to stderr in a single `write`, like `eprintln!` but
/// line-atomic against other processes appending to the same file. A failed
/// write is ignored rather than panicking.
#[macro_export]
macro_rules! log_line {
    ($($arg:tt)*) => {
        $crate::log::write_line(::std::format_args!($($arg)*))
    };
}

/// The body of [`log_line!`](crate::log_line): formats `args` plus `\n` into
/// one buffer and writes it with one `write_all` under the stderr lock.
#[doc(hidden)]
pub fn write_line(args: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let mut line = std::fmt::format(args);
    line.push('\n');
    // One write(2) under the stderr lock. A failed log write is ignored: a
    // diagnostic must never take the process down (eprintln! panics).
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
}
