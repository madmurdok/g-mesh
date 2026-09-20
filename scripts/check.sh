#!/usr/bin/env bash
# The formatting and lint gates, defined once so a local run and CI cannot
# disagree about what they check.
#
# They disagreed until GM-368. Every task in the 3.8.0 batch ran
# `cargo clippy -p g-mesh --lib -- -D warnings`, because that is what the task
# prompts asked for; CI runs `--workspace --all-targets`, which additionally
# compiles test targets. Four lints in a `#[cfg(test)]` module therefore passed
# six local runs and left the release branch unable to pass its own CI. The
# fix is not to remember harder - it is for both callers to run this file.
#
# `.github/workflows/ci.yml`'s two steps call it rather than repeating the
# commands, so a change here reaches CI and a change in CI is impossible to
# make without touching here. The steps stay separate on purpose: `fmt` is the
# cheaper failure and the one with an obvious remedy, so CI reports it first.
#
#   scripts/check.sh          both, fmt first
#   scripts/check.sh fmt      formatting only
#   scripts/check.sh clippy   lints only
set -euo pipefail

cd "$(dirname "$0")/.."

# `--all`: every workspace member, not core alone (GM-284).
run_fmt() {
    echo "== cargo fmt --all --check"
    cargo fmt --all --check
}

# `-D warnings`, because a lint that reports and passes is a lint nobody reads.
# `--all-targets` is the half that was being skipped: it compiles tests,
# benches and examples, where a `#[cfg(test)]` module's lints live.
run_clippy() {
    echo "== cargo clippy --workspace --all-targets -- -D warnings"
    cargo clippy --workspace --all-targets -- -D warnings
}

case "${1:-all}" in
fmt) run_fmt ;;
clippy) run_clippy ;;
all)
    run_fmt
    run_clippy
    ;;
*)
    echo "usage: $0 [fmt|clippy|all]" >&2
    exit 2
    ;;
esac
