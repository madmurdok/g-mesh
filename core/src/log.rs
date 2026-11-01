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

/// The variable that turns on [`log_timing!`](crate::log_timing) lines (GM-435):
/// any non-empty value. Plugins inherit the daemon's environment, so one
/// variable turns on both sides.
pub const TIMING_ENV: &str = "G_MESH_EDIT_TIMING";

/// The fixed start of every timing line, before its `t=` stamp.
pub const TIMING_PREFIX: &str = "g-mesh timing: ";

/// Whether [`TIMING_ENV`] is set to a non-empty value: read once, then a cached
/// bool, so an unset variable costs one load per call site and nothing else.
pub fn timing_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| timing_enabled_from(std::env::var_os(TIMING_ENV).as_deref()))
}

/// [`timing_enabled`]'s rule over a given value of [`TIMING_ENV`]: set and
/// non-empty.
pub fn timing_enabled_from(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

/// One timing line without its newline:
/// `g-mesh timing: t=<unix seconds>.<micros, 6 digits> <args>`.
pub fn timing_line(now: std::time::SystemTime, args: std::fmt::Arguments<'_>) -> String {
    let since = now.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    format!("{TIMING_PREFIX}t={}.{:06} {args}", since.as_secs(), since.subsec_micros())
}

/// Writes one phase-boundary timing line (GM-435) when [`TIMING_ENV`] is set,
/// and does nothing, not even format its arguments, when it is not. The
/// format string starts with the event name, then `key=value` pairs:
/// `log_timing!("rt_sent id={id} method={method}")`. Stamped from the
/// wall clock when written, so lines from the daemon and its plugins share
/// one clock.
#[macro_export]
macro_rules! log_timing {
    ($($arg:tt)*) => {
        if $crate::log::timing_enabled() {
            $crate::log::write_timing(::std::format_args!($($arg)*))
        }
    };
}

/// The body of [`log_timing!`](crate::log_timing) once enabled: stamps `args`
/// with the current time and writes it as one line.
#[doc(hidden)]
pub fn write_timing(args: std::fmt::Arguments<'_>) {
    let line = timing_line(std::time::SystemTime::now(), args);
    write_line(format_args!("{line}"));
}

#[cfg(test)]
mod timing_tests {
    use std::ffi::OsStr;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::{timing_enabled_from, timing_line, TIMING_ENV, TIMING_PREFIX};

    /// Splits a timing line into its `t=` stamp and the rest, checking the
    /// stamp is `<unix seconds>.<exactly 6 digits>`.
    fn stamp_and_rest(line: &str) -> (Duration, &str) {
        let rest = line.strip_prefix(TIMING_PREFIX).unwrap_or_else(|| panic!("no prefix: {line:?}"));
        let rest =
            rest.strip_prefix("t=").unwrap_or_else(|| panic!("no t= right after the prefix: {line:?}"));
        let (stamp, rest) = rest.split_once(' ').unwrap_or_else(|| panic!("nothing after t=: {line:?}"));
        let (secs, micros) = stamp.split_once('.').unwrap_or_else(|| panic!("t= has no fraction: {line:?}"));
        assert!(!secs.is_empty() && secs.bytes().all(|b| b.is_ascii_digit()), "seconds not digits: {line:?}");
        assert!(micros.len() == 6 && micros.bytes().all(|b| b.is_ascii_digit()), "not 6 digits: {line:?}");
        (Duration::from_secs(secs.parse().unwrap()) + Duration::from_micros(micros.parse().unwrap()), rest)
    }

    #[test]
    fn timing_is_off_when_the_variable_is_unset_or_empty() {
        assert!(!timing_enabled_from(None));
        assert!(!timing_enabled_from(Some(OsStr::new(""))));
    }

    #[test]
    fn timing_is_on_for_any_non_empty_value() {
        for value in ["1", "0", "false", " "] {
            assert!(
                timing_enabled_from(Some(OsStr::new(value))),
                "{TIMING_ENV}={value:?} must turn timing on"
            );
        }
    }

    #[test]
    fn timing_line_is_prefix_unix_stamp_event_then_fields_in_order() {
        let at = UNIX_EPOCH + Duration::from_secs(1_700_000_000) + Duration::from_nanos(1_234_999);
        let line =
            timing_line(at, format_args!("rt_sent id={} method={} file={}", 7, "fileChanged", "src/a.rs"));
        assert_eq!(line, "g-mesh timing: t=1700000000.001234 rt_sent id=7 method=fileChanged file=src/a.rs");
        assert!(!line.contains('\n'), "the line carries no newline; write_line adds it");

        let (stamp, rest) = stamp_and_rest(&line);
        assert_eq!(stamp, Duration::from_micros(1_700_000_000_001_234));
        let mut tokens = rest.split(' ');
        assert_eq!(tokens.next(), Some("rt_sent"), "the event comes first");
        let fields: Vec<_> = tokens.map(|token| token.split_once('=').expect("key=value")).collect();
        assert_eq!(fields, [("id", "7"), ("method", "fileChanged"), ("file", "src/a.rs")]);
    }

    #[test]
    fn timing_line_pads_micros_to_six_digits() {
        let line = timing_line(
            UNIX_EPOCH + Duration::from_secs(5) + Duration::from_micros(7),
            format_args!("settled"),
        );
        assert_eq!(line, "g-mesh timing: t=5.000007 settled");
        let line = timing_line(UNIX_EPOCH + Duration::from_secs(5), format_args!("settled"));
        assert_eq!(line, "g-mesh timing: t=5.000000 settled");
    }

    #[test]
    fn timing_line_stamps_now_as_unix_seconds() {
        let before = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let line = timing_line(SystemTime::now(), format_args!("route file=a.ts language=typescript"));
        let after = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let (stamp, rest) = stamp_and_rest(&line);
        // Truncated to whole microseconds, so compare at that resolution.
        let floor = Duration::from_micros(before.as_micros() as u64);
        assert!(floor <= stamp && stamp <= after, "{stamp:?} outside [{floor:?}, {after:?}]");
        assert_eq!(rest, "route file=a.ts language=typescript");
    }
}
