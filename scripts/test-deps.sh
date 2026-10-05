#!/usr/bin/env bash
# The test dependencies `cargo test --workspace` needs that cargo cannot fetch
# itself, installed the way CI installs them - so a fresh clone or worktree
# gets a green suite from one command instead of from folklore.
#
# Until GM-419 the suite was green only where someone had once run
# `npm install pyright` in plugins/python by hand: nothing in the repository
# recorded that it was needed, and CI's own step floated on whatever pyright
# npm called latest that day. The pyright step below is the one
# `.github/workflows/ci.yml` runs, so a local setup and CI install the same
# thing - the same arrangement `scripts/check.sh` has for fmt and clippy.
#
#   scripts/test-deps.sh               both, in this order
#   scripts/test-deps.sh rust-analyzer the rustup component plugins/rust tests drive
#   scripts/test-deps.sh pyright       npm ci for plugins/python (pyright, its test dependency)
#
# pyright's version is pinned: an exact version in plugins/python/package.json,
# locked in the committed package-lock.json, installed with `npm ci`. Bumping it is a
# deliberate act with its own diff, like the ShellCheck and Rust pins in
# ci.yml: a new pyright can change what the Python plugin's semantic tier
# answers, and plugins/python/conformance/expect.toml is written against the
# answers of this one.
set -euo pipefail

cd "$(dirname "$0")/.."

# `rust-analyzer --version`, not `which`: `~/.cargo/bin/rust-analyzer` is a
# rustup proxy that exists whether or not the component behind it does (see
# ci.yml's "Install rust-analyzer" step).
install_rust_analyzer() {
    echo "== rustup component add rust-analyzer"
    rustup component add rust-analyzer
    rust-analyzer --version
}

# `npm ci`: it installs exactly what
# plugins/python/package-lock.json records and fails if package.json and the
# lock disagree. `pyright --version` is the check because `pyright-langserver`
# has no such flag; the version it prints is compared with the one package.json
# declares, read from there rather than repeated here, so the pin has one home.
install_pyright() {
    echo "== npm ci (plugins/python)"
    npm ci --prefix plugins/python
    local expected reported
    expected="$(node -p 'require("./plugins/python/package.json").devDependencies.pyright')"
    reported="$(plugins/python/node_modules/.bin/pyright --version)"
    echo "$reported"
    if [ "$reported" != "pyright ${expected}" ]; then
        echo "expected pyright ${expected} (plugins/python/package.json), got: ${reported}" >&2
        exit 1
    fi
}

case "${1:-all}" in
rust-analyzer) install_rust_analyzer ;;
pyright) install_pyright ;;
all)
    install_rust_analyzer
    install_pyright
    ;;
*)
    echo "usage: $0 [rust-analyzer|pyright|all]" >&2
    exit 2
    ;;
esac
