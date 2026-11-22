//! Every `G_MESH_*` environment variable the core reads or sets, in one
//! table, and the shared readers that parse them.
//!
//! A leaf module: it depends on `std` and [`crate::log_role!`] only, so any
//! module can read a knob without creating an import cycle. Each module keeps
//! its `*_ENV` constant as an alias of the row's `name`.
//!
//! Outside this file nothing reads the process environment, except
//! `CLAUDE_PROJECT_DIR` in `shim.rs`; `every_env_read_goes_through_the_knob_table`
//! enforces it.

use std::ffi::{OsStr, OsString};
use std::str::FromStr;
use std::time::Duration;

/// Who a knob is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Documented for people running g-mesh.
    User,
    /// Performance or behaviour tuning.
    Tuning,
    /// Read only so that tests can steer a live process.
    TestHook,
    /// Set by the core for a child plugin process; the core never reads it.
    Child,
}

/// One environment variable.
#[derive(Debug, Clone, Copy)]
pub struct Knob {
    pub name: &'static str,
    pub kind: Kind,
    pub doc: &'static str,
}

/// What a `0` means for a millisecond knob read by [`millis`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZeroIs {
    /// `None`: the timer is off.
    Off,
    /// The default: zero is not a meaningful value.
    Default,
}

const fn knob(name: &'static str, kind: Kind, doc: &'static str) -> Knob {
    Knob { name, kind, doc }
}

pub const HOME: Knob = knob("G_MESH_HOME", Kind::User, "state root; empty is unset");
pub const DAEMON_LOG: Knob =
    knob("G_MESH_DAEMON_LOG", Kind::User, "file the detached daemon's stderr goes to; empty is unset");
pub const MODEL_DIR: Knob =
    knob("G_MESH_MODEL_DIR", Kind::User, "embedding model directory; empty is the empty path");
pub const RERANK_MODEL_DIR: Knob =
    knob("G_MESH_RERANK_MODEL_DIR", Kind::User, "cross-encoder directory; empty is unset");
pub const RERANK: Knob = knob("G_MESH_RERANK", Kind::User, "`off` switches the search rerank off");
pub const RERANK_MODEL: Knob =
    knob("G_MESH_RERANK_MODEL", Kind::User, "`l6` or `qat`; the rerank model, over `[rerank] model`");
pub const EMBEDDING_CACHE: Knob =
    knob("G_MESH_EMBEDDING_CACHE", Kind::User, "`off` switches the embedding cache off");
pub const SEED: Knob = knob("G_MESH_SEED", Kind::User, "`0` or `1` overrides the seeding default");
pub const MODEL_BASE_URL: Knob = knob(
    "G_MESH_MODEL_BASE_URL",
    Kind::User,
    "mirror `model fetch` tries before the built-in sources; empty is unset",
);
pub const DOWNLOAD_BASE: Knob =
    knob("G_MESH_DOWNLOAD_BASE", Kind::User, "release download base URL; empty is unset");
pub const REPO: Knob =
    knob("G_MESH_REPO", Kind::User, "GitHub `owner/name` the release is downloaded from; empty is unset");

pub const RERANK_BETA: Knob = knob(
    "G_MESH_RERANK_BETA",
    Kind::Tuning,
    "the rerank blend's cosine weight, over the rerank model's own; a finite number >= 0",
);
pub const EMBED_INTRA_THREADS: Knob =
    knob("G_MESH_EMBED_INTRA_THREADS", Kind::Tuning, "intra-op threads of the embedding session");
pub const EMBED_INTER_THREADS: Knob =
    knob("G_MESH_EMBED_INTER_THREADS", Kind::Tuning, "inter-op threads of the embedding session");
pub const EMBED_OPT_LEVEL: Knob =
    knob("G_MESH_EMBED_OPT_LEVEL", Kind::Tuning, "graph optimization level of the embedding session");
pub const EMBED_SPIN: Knob = knob("G_MESH_EMBED_SPIN", Kind::Tuning, "`0` or `1`: intra-op thread spinning");
pub const EMBED_BATCH: Knob = knob("G_MESH_EMBED_BATCH", Kind::Tuning, "chunks per embedding batch");
pub const EMBED_TOKEN_BUDGET: Knob =
    knob("G_MESH_EMBED_TOKEN_BUDGET", Kind::Tuning, "tokens per embedding batch");
pub const EMBED_BUCKET: Knob =
    knob("G_MESH_EMBED_BUCKET", Kind::Tuning, "`0` or `1`: bucket chunks by length");
pub const EDIT_TIMING: Knob =
    knob("G_MESH_EDIT_TIMING", Kind::Tuning, "any non-empty value turns on timing lines");
pub const TRACE_CALLS: Knob =
    knob("G_MESH_TRACE_CALLS", Kind::Tuning, "any non-empty value turns on tool-call trace lines");
pub const PROGRESS_INTERVAL_MS: Knob =
    knob("G_MESH_PROGRESS_INTERVAL_MS", Kind::Tuning, "progress notification interval; 0 sends none");
pub const INDEX_WAIT_CAP_MS: Knob =
    knob("G_MESH_INDEX_WAIT_CAP_MS", Kind::Tuning, "longest wait for the index; 0 removes the cap");
pub const SEARCH_EMBEDDING_WAIT_MS: Knob = knob(
    "G_MESH_SEARCH_EMBEDDING_WAIT_MS",
    Kind::Tuning,
    "longest `search_code` wait for the embedding pass; 0 does not wait",
);

pub const PLUGIN_ROOTS_OVERRIDE: Knob = knob(
    "G_MESH_PLUGIN_ROOTS_OVERRIDE",
    Kind::TestHook,
    "the one plugin root, replacing discovery's; empty is the empty path",
);
pub const BOOTSTRAP_TIMEOUT_MS: Knob =
    knob("G_MESH_BOOTSTRAP_TIMEOUT_MS", Kind::TestHook, "the shim's daemon bootstrap timeout");
pub const PLUGIN_IDLE_MS: Knob =
    knob("G_MESH_PLUGIN_IDLE_MS", Kind::TestHook, "plugin idle timeout; 0 turns it off");
pub const CORE_IDLE_MS: Knob =
    knob("G_MESH_CORE_IDLE_MS", Kind::TestHook, "core idle timeout; 0 turns it off");
pub const FRONT_IDLE_MS: Knob =
    knob("G_MESH_FRONT_IDLE_MS", Kind::TestHook, "front idle timeout; 0 turns it off");
pub const FILE_CHANGED_TIMEOUT_MS: Knob = knob(
    "G_MESH_FILE_CHANGED_TIMEOUT_MS",
    Kind::TestHook,
    "fileChanged round-trip timeout; 0 is the default",
);
pub const SEMANTIC_PASS_FILE_TIMEOUT_MS: Knob = knob(
    "G_MESH_SEMANTIC_PASS_FILE_TIMEOUT_MS",
    Kind::TestHook,
    "per-file semanticPass round-trip timeout; 0 is the default",
);
pub const SEMANTIC_PASS_PROJECT_TIMEOUT_MS: Knob = knob(
    "G_MESH_SEMANTIC_PASS_PROJECT_TIMEOUT_MS",
    Kind::TestHook,
    "whole-project semanticPass timeout floor; 0 is the default",
);
pub const LIFELINE_PID: Knob = knob("G_MESH_LIFELINE_PID", Kind::TestHook, "pid whose death ends the daemon");
pub const BULK_INDEX_DELAY_MS: Knob =
    knob("G_MESH_BULK_INDEX_DELAY_MS", Kind::TestHook, "holds the finished bulk walk open this long");
pub const BULK_INDEX_HOLD_FILE: Knob = knob(
    "G_MESH_BULK_INDEX_HOLD_FILE",
    Kind::TestHook,
    "holds the finished bulk walk until this file is removed",
);
pub const BULK_INDEX_NO_ABSENT_COUNT: Knob =
    knob("G_MESH_BULK_INDEX_NO_ABSENT_COUNT", Kind::TestHook, "any non-empty value skips the absent count");
pub const BULK_INDEX_HOLD_LOCK_FILE: Knob = knob(
    "G_MESH_BULK_INDEX_HOLD_LOCK_FILE",
    Kind::TestHook,
    "holds the index write lock until this file is removed",
);
pub const ROUND_TRIP_HOLD_COMPUTE_FILE: Knob = knob(
    "G_MESH_ROUND_TRIP_HOLD_COMPUTE_FILE",
    Kind::TestHook,
    "holds a round trip's compute until this file is removed",
);
pub const EMBED_PASS_HOLD_FILE: Knob = knob(
    "G_MESH_EMBED_PASS_HOLD_FILE",
    Kind::TestHook,
    "holds the embedding pass before its first batch until this file is removed",
);
pub const EMBED_PASS_PANIC: Knob = knob(
    "G_MESH_EMBED_PASS_PANIC",
    Kind::TestHook,
    "any non-empty value panics the embedding pass before its first batch",
);
pub const SEED_HOLD_RECONCILE_FILE: Knob = knob(
    "G_MESH_SEED_HOLD_RECONCILE_FILE",
    Kind::TestHook,
    "holds a seeded start's reconcile until this file is removed",
);

pub const BULK_STDIN_LIFELINE: Knob = knob(
    "G_MESH_BULK_STDIN_LIFELINE",
    Kind::Child,
    "set for a bulk-index child so it exits when its stdin closes",
);
pub const PLUGIN_MANIFEST: Knob =
    knob("G_MESH_PLUGIN_MANIFEST", Kind::Child, "manifest path handed to a plugin process");
pub const PLUGIN_CHECK_MARKER_DIR: Knob = knob(
    "G_MESH_PLUGIN_CHECK_MARKER_DIR",
    Kind::Child,
    "directory a plugin check's child writes its markers to",
);

/// Every knob, once.
pub const ALL: &[Knob] = &[
    HOME,
    DAEMON_LOG,
    MODEL_DIR,
    RERANK_MODEL_DIR,
    RERANK,
    RERANK_MODEL,
    EMBEDDING_CACHE,
    SEED,
    MODEL_BASE_URL,
    DOWNLOAD_BASE,
    REPO,
    RERANK_BETA,
    EMBED_INTRA_THREADS,
    EMBED_INTER_THREADS,
    EMBED_OPT_LEVEL,
    EMBED_SPIN,
    EMBED_BATCH,
    EMBED_TOKEN_BUDGET,
    EMBED_BUCKET,
    EDIT_TIMING,
    TRACE_CALLS,
    PROGRESS_INTERVAL_MS,
    INDEX_WAIT_CAP_MS,
    SEARCH_EMBEDDING_WAIT_MS,
    PLUGIN_ROOTS_OVERRIDE,
    BOOTSTRAP_TIMEOUT_MS,
    PLUGIN_IDLE_MS,
    CORE_IDLE_MS,
    FRONT_IDLE_MS,
    FILE_CHANGED_TIMEOUT_MS,
    SEMANTIC_PASS_FILE_TIMEOUT_MS,
    SEMANTIC_PASS_PROJECT_TIMEOUT_MS,
    LIFELINE_PID,
    BULK_INDEX_DELAY_MS,
    BULK_INDEX_HOLD_FILE,
    BULK_INDEX_NO_ABSENT_COUNT,
    BULK_INDEX_HOLD_LOCK_FILE,
    ROUND_TRIP_HOLD_COMPUTE_FILE,
    EMBED_PASS_HOLD_FILE,
    EMBED_PASS_PANIC,
    SEED_HOLD_RECONCILE_FILE,
    BULK_STDIN_LIFELINE,
    PLUGIN_MANIFEST,
    PLUGIN_CHECK_MARKER_DIR,
];

/// The raw value of the variable `name`, `None` when unset or not UTF-8.
pub fn lookup(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The raw value of the variable `name`, `None` when unset.
pub fn lookup_os(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// The value as set, an empty value included.
pub fn os_raw(knob: &Knob) -> Option<OsString> {
    std::env::var_os(knob.name)
}

/// The value when set and not empty.
pub fn os_non_empty(knob: &Knob) -> Option<OsString> {
    os_raw(knob).filter(|value| !value.is_empty())
}

/// The value as set when it is UTF-8, an empty value included.
pub fn string_raw(knob: &Knob) -> Option<String> {
    lookup(knob.name)
}

/// The value when set, UTF-8 and not empty.
pub fn string_non_empty(knob: &Knob) -> Option<String> {
    string_raw(knob).filter(|value| !value.is_empty())
}

/// [`flag`]'s rule over a given value: set and not empty.
pub fn flag_from(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

/// Whether the knob is set to a non-empty value.
pub fn flag(knob: &Knob) -> bool {
    flag_from(os_raw(knob).as_deref())
}

/// [`off_switch`]'s rule over a given value: `off`, in any case.
pub fn is_off(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| value.eq_ignore_ascii_case("off"))
}

/// Whether the knob is set to `off`; anything else, empty included, leaves the
/// feature on.
pub fn off_switch(knob: &Knob) -> bool {
    is_off(os_raw(knob).as_deref())
}

/// [`parsed`]'s rule over a given value: trimmed, `None` when unset or not a `T`.
pub fn parsed_from<T: FromStr>(raw: Option<&str>) -> Option<T> {
    raw?.trim().parse().ok()
}

/// The value parsed as a `T`; unset, empty or unparsable is `None`, silently.
pub fn parsed<T: FromStr>(knob: &Knob) -> Option<T> {
    parsed_from(string_raw(knob).as_deref())
}

/// [`millis`]'s rule over a given raw value; `name` is only for the log line.
pub fn millis_from(raw: Option<&str>, name: &str, zero: ZeroIs, default: Duration) -> Option<Duration> {
    let Some(raw) = raw else { return Some(default) };
    match raw.trim().parse::<u64>() {
        Ok(0) => match zero {
            ZeroIs::Off => None,
            ZeroIs::Default => Some(default),
        },
        Ok(millis) => Some(Duration::from_millis(millis)),
        Err(_) => {
            crate::log_role!(
                "ignoring {name}={raw:?} - not a whole number of milliseconds; \
                 using the default {default:?}"
            );
            Some(default)
        }
    }
}

/// A millisecond count. Unset gives `default`; so does a value that is not a
/// whole number, with one log line, so a typo neither changes a timer
/// silently nor stops the process starting. `0` follows `zero`; with
/// [`ZeroIs::Default`] the result is always `Some`.
pub fn millis(knob: &Knob, zero: ZeroIs, default: Duration) -> Option<Duration> {
    millis_from(string_raw(knob).as_deref(), knob.name, zero, default)
}

/// A millisecond count read per call. Unset, empty or unparsable gives
/// `default`, silently; `0` is [`Duration::ZERO`] and the consumer decides
/// what that means.
pub fn millis_lenient(knob: &Knob, default: Duration) -> Duration {
    parsed::<u64>(knob).map_or(default, Duration::from_millis)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::Path;

    use super::*;

    /// Files and directories under `src` that are test code as a whole.
    fn is_test_path(path: &Path) -> bool {
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
        name == "tests" || name == "tests.rs" || name.ends_with("_tests.rs") || name == "test_plugin.rs"
    }

    fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if is_test_path(&path) {
                continue;
            }
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// The lines of `source` outside `#[cfg(test)]` items. An item ends at the
    /// brace that closes its first brace, or at a `;` before any brace.
    fn non_test_lines(source: &str) -> Vec<(usize, &str)> {
        let mut kept = Vec::new();
        let mut skipping = false;
        let (mut depth, mut seen_brace) = (0i32, false);
        for (index, line) in source.lines().enumerate() {
            let trimmed = line.trim_start();
            if !skipping && trimmed.starts_with("#[cfg(test)]") {
                skipping = true;
                (depth, seen_brace) = (0, false);
                continue;
            }
            if skipping {
                if trimmed.starts_with("#[") || trimmed.starts_with("///") {
                    continue;
                }
                for ch in line.chars() {
                    match ch {
                        '{' => {
                            depth += 1;
                            seen_brace = true;
                        }
                        '}' => depth -= 1,
                        _ => {}
                    }
                }
                if (seen_brace && depth <= 0) || (!seen_brace && trimmed.ends_with(';')) {
                    skipping = false;
                }
                continue;
            }
            kept.push((index + 1, line));
        }
        kept
    }

    #[test]
    fn millis_follows_its_zero_rule_and_falls_back_on_unset_or_bad_values() {
        let default = Duration::from_secs(30);
        let cases: [(Option<&str>, ZeroIs, Option<Duration>); 10] = [
            (None, ZeroIs::Off, Some(default)),
            (None, ZeroIs::Default, Some(default)),
            (Some("0"), ZeroIs::Off, None),
            (Some(" 0 "), ZeroIs::Off, None),
            (Some("0"), ZeroIs::Default, Some(default)),
            (Some(" 250 "), ZeroIs::Off, Some(Duration::from_millis(250))),
            (Some("250"), ZeroIs::Default, Some(Duration::from_millis(250))),
            (Some("10 minutes"), ZeroIs::Off, Some(default)),
            (Some("-1"), ZeroIs::Default, Some(default)),
            (Some(""), ZeroIs::Off, Some(default)),
        ];
        for (raw, zero, expected) in cases {
            assert_eq!(millis_from(raw, "G_MESH_X_MS", zero, default), expected, "{raw:?} under {zero:?}");
        }
    }

    /// The rule `parsed` and `millis_lenient` read with: `0` is a value, not
    /// a fallback.
    #[test]
    fn parsed_reads_a_trimmed_value_and_nothing_else() {
        assert_eq!(parsed_from::<u64>(Some("0")), Some(0));
        assert_eq!(parsed_from::<u64>(Some(" 1500 ")), Some(1500));
        assert_eq!(parsed_from::<u64>(Some("abc")), None);
        assert_eq!(parsed_from::<u64>(Some("")), None);
        assert_eq!(parsed_from::<u64>(None), None);
    }

    #[test]
    fn off_and_flag_rules() {
        assert!(is_off(Some(OsStr::new("off"))));
        assert!(is_off(Some(OsStr::new("Off"))));
        assert!(!is_off(Some(OsStr::new(""))));
        assert!(!is_off(Some(OsStr::new("0"))));
        assert!(!is_off(None));
        assert!(flag_from(Some(OsStr::new("0"))));
        assert!(flag_from(Some(OsStr::new("off"))));
        assert!(!flag_from(Some(OsStr::new(""))));
        assert!(!flag_from(None));
    }

    #[test]
    fn every_env_read_goes_through_the_knob_table() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        assert!(files.len() > 50, "the scan found only {} files under {}", files.len(), src.display());

        let mut stray = Vec::new();
        for path in files {
            if path.file_name().is_some_and(|name| name == "knobs.rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            for (line_no, line) in non_test_lines(&source) {
                let code = line.trim_start();
                if code.starts_with("//") {
                    continue;
                }
                let reads_env =
                    ["env::var(", "env::var_os(", "var_os("].iter().any(|needle| code.contains(needle));
                let allowed = path.ends_with("shim.rs") && code.contains("PROJECT_DIR_ENV");
                if reads_env && !allowed {
                    stray.push(format!("{}:{line_no}: {code}", path.strip_prefix(&src).unwrap().display()));
                }
            }
        }
        assert!(stray.is_empty(), "environment reads outside knobs.rs:\n{}", stray.join("\n"));

        let mut seen = HashSet::new();
        for knob in ALL {
            assert!(knob.name.starts_with("G_MESH_"), "{} must start with G_MESH_", knob.name);
            assert!(seen.insert(knob.name), "{} is listed twice", knob.name);
        }
    }
}
