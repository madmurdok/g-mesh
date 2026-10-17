//! The suite's own wait budgets stay below nextest's per-test limit.
//!
//! A wait that outlasts `slow-timeout` (`period * terminate-after` in
//! `.config/nextest.toml`'s default profile) is killed by nextest before it
//! can fail, and its panic - the one naming what it waited for - is never
//! shown. Both numbers live in different files, so nothing but this test
//! keeps them in step.

use std::path::Path;
use std::time::Duration;

use crate::common;

/// The default profile's `slow-timeout`, as `period * terminate-after`.
fn nextest_per_test_limit() -> Duration {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../.config/nextest.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    let config: toml::Value =
        text.parse().unwrap_or_else(|err| panic!("{} is not TOML: {err}", path.display()));
    let slow = &config["profile"]["default"]["slow-timeout"];
    let period = slow["period"].as_str().expect("slow-timeout.period must be a string such as \"30s\"");
    let period_secs: u64 = period
        .strip_suffix('s')
        .and_then(|secs| secs.parse().ok())
        .unwrap_or_else(|| panic!("slow-timeout.period {period:?} is not a whole number of seconds"));
    let terminate_after =
        slow["terminate-after"].as_integer().expect("slow-timeout.terminate-after must be an integer");
    Duration::from_secs(
        period_secs * u64::try_from(terminate_after).expect("terminate-after must not be negative"),
    )
}

#[test]
fn every_default_wait_budget_is_below_nextests_per_test_limit() {
    let limit = nextest_per_test_limit();
    for (name, secs) in [
        ("DEFAULT_STARTUP_TIMEOUT_SECS", common::DEFAULT_STARTUP_TIMEOUT_SECS),
        ("DEFAULT_INDEXED_TIMEOUT_SECS", common::DEFAULT_INDEXED_TIMEOUT_SECS),
    ] {
        assert!(
            Duration::from_secs(secs) < limit,
            "{name} ({secs}s) must stay below nextest's per-test limit ({limit:?}), or nextest kills \
             the wait before it can fail with its own message"
        );
    }
}
