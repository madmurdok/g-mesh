#!/usr/bin/env bash
# The test dependencies `cargo test --workspace` needs that cargo cannot fetch
# itself, installed the way CI installs them - so a fresh clone or worktree
# gets a green suite from one command instead of from folklore.
#
# Until GM-419 the suite was green only where someone had once run
# `npm install pyright` in plugins/python by hand: that install is gitignored
# (plugins/python/.gitignore says why), so nothing in the repository recorded
# that it was needed, and CI's own step floated on whatever pyright npm called
# latest that day. The pyright step below is the one `.github/workflows/ci.yml`
# runs, and the version is pinned here, once, for both callers - the same
# arrangement `scripts/check.sh` has for fmt and clippy.
#
#   scripts/test-deps.sh               all three, in this order
#   scripts/test-deps.sh typescript    npm ci for the JS/TS plugin (build.rs builds it)
#   scripts/test-deps.sh rust-analyzer the rustup component plugins/rust tests drive
#   scripts/test-deps.sh pyright       pyright into plugins/python/node_modules
#
# Bumping PYRIGHT_VERSION is a deliberate act with its own diff, like the
# ShellCheck and Rust pins in ci.yml: a new pyright can change what the Python
# plugin's semantic tier answers, and plugins/python/conformance/expect.toml
# is written against the answers of this one.
set -euo pipefail

PYRIGHT_VERSION=1.1.414

cd "$(dirname "$0")/.."

install_typescript() {
    echo "== npm ci (plugins/typescript)"
    npm ci --prefix plugins/typescript
}

# `rust-analyzer --version`, not `which`: `~/.cargo/bin/rust-analyzer` is a
# rustup proxy that exists whether or not the component behind it does (see
# ci.yml's "Install rust-analyzer" step).
install_rust_analyzer() {
    echo "== rustup component add rust-analyzer"
    rustup component add rust-analyzer
    rust-analyzer --version
}

# `--prefix plugins/python`, not a `cd`: with no package.json there, npm would
# otherwise pick the nearest ancestor holding one as the install root.
# `--no-save`: plugins/python keeps no package.json or lockfile (gitignored),
# and this pin is the record of which pyright the suite runs against.
# `pyright --version` is the check because `pyright-langserver` has no such
# flag; the version it prints is compared, so the pin is enforced rather than
# merely requested.
install_pyright() {
    echo "== npm install pyright@${PYRIGHT_VERSION} (plugins/python)"
    npm install --no-save --prefix plugins/python "pyright@${PYRIGHT_VERSION}"
    local reported
    reported="$(plugins/python/node_modules/.bin/pyright --version)"
    echo "$reported"
    if [ "$reported" != "pyright ${PYRIGHT_VERSION}" ]; then
        echo "expected pyright ${PYRIGHT_VERSION}, got: ${reported}" >&2
        exit 1
    fi
}

case "${1:-all}" in
typescript) install_typescript ;;
rust-analyzer) install_rust_analyzer ;;
pyright) install_pyright ;;
all)
    install_typescript
    install_rust_analyzer
    install_pyright
    ;;
*)
    echo "usage: $0 [typescript|rust-analyzer|pyright|all]" >&2
    exit 2
    ;;
esac
