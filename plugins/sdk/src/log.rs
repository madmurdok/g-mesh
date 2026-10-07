//! Line-atomic stderr logging for plugins.
//!
//! # The contract for plugin authors
//!
//! **Every stderr line goes out in one `write`: the daemon log is shared by
//! the daemon and every plugin.** A plugin's stderr is inherited from the
//! daemon, whose stderr is the project's `daemon.log`, opened `O_APPEND`. A
//! line survives intact only if it reaches the file in a single `write(2)`;
//! `eprintln!` does one write per format piece, so the daemon's lines and a
//! plugin's can split each other. Log through
//! [`log_line!`](crate::log_line) instead, which formats the whole line,
//! newline included, and writes it once.
//!
//! `docs/architecture/gm-520-line-atomic-logs.md` is the design. Core carries
//! an identical copy (`g_mesh::log_line!`), since this crate does not depend
//! on core.

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
