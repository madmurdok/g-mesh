#!/usr/bin/env bash
#
# Cross-compiles the bundled Go plugin (plugins/go) for one release target
# and stages it, with a manifest naming its own binary, as a `go/` plugin
# directory core can discover next to its own executable.
#
#   scripts/bundle-go-plugin.sh                                   # host target, into dist/plugins
#   scripts/bundle-go-plugin.sh x86_64-pc-windows-msvc /tmp/stage # explicit target and destination
#
# `scripts/build-targets.sh` calls this while staging a release archive; it
# is runnable on its own so the bundle can be built and inspected without
# packaging a whole release.
#
# ---------------------------------------------------------------------------
# WHY THIS NEEDS NO NATIVE RUNNER, UNLIKE scripts/bundle-plugin.sh
#
# The JS/TS plugin embeds the host's own Node runtime (Node SEA), so it can
# only be built on the platform it targets - that is why build-targets.sh
# builds each release target on its own runner. The Go plugin carries no
# runtime at all: `CGO_ENABLED=0` plus `GOOS`/`GOARCH` produces a static,
# self-contained binary for any target from any host (see
# docs/architecture/multi-language-plugins.md's Go plugin "Distribution"
# line), so this script - unlike bundle-plugin.sh - never refuses a
# non-host target.
#
# ---------------------------------------------------------------------------
# THE WINDOWS GAP THIS SCRIPT CLOSES (GM-283)
#
# plugins/go/plugin.toml - the repo's own, dev-checkout manifest - spawns
# `./g-mesh-plugin-go`, with no `.exe`, because core/build.rs builds under
# that exact literal name on every platform (see that file's own comment)
# and a single checked-in manifest cannot vary its text by the platform it
# happens to be read on.
#
# Verified locally before assuming anything: `GOOS=windows GOARCH=amd64
# CGO_ENABLED=0 go build -o g-mesh-plugin-go .` produces a file named
# exactly `g-mesh-plugin-go` (a Windows PE binary, no extension) - Go only
# appends `.exe` on its own when `-o` is *omitted*, not when it is given
# explicitly. And `core/src/daemon/manifest.rs`'s `resolve_path_entry`
# performs no extension handling of its own; it only decides bare-command
# vs. path-relative-to-the-manifest-directory.
#
# Also checked (library/std/src/sys/process/windows.rs `resolve_exe`, the
# rustc `rust-src` component): Rust's own process spawn resolver appends
# `.exe` to a path-shaped program name and, if that file does not exist,
# falls back to the literal name unmodified - which Windows can in fact
# execute given its full path (the loader identifies a PE image by its
# header, not its filename extension). So the unmodified manifest is not
# provably broken on Windows. But shipping a release that depends on that
# fallback - rather than on a manifest whose `command` names the file that
# is actually staged beside it - is not a bet worth making for a release
# archive, so this script generates a manifest whose `command` matches its
# own target's real binary name, the same way bundle-plugin.sh writes an
# installed-specific manifest for the TS plugin rather than reusing the
# checkout's unmodified one.
#
# The generated manifest is derived from plugins/go/plugin.toml by rewriting
# only the `[plugin.spawn] command` line, not hand-duplicated field-by-field
# the way bundle-plugin.sh's TS manifest is: the Go plugin's installed and
# dev-checkout manifests agree on every other field, so one substitution
# keeps them from drifting apart instead of restating them twice.
#
# Environment:
#   GO_BIN  the Go binary to build with (default: `go` on PATH)
# ---------------------------------------------------------------------------
#
# WHAT ENDS UP IN THE STAGED DIRECTORY
#
#   go/
#     plugin.toml              discovery manifest; spawns the binary below
#     g-mesh-plugin-go[.exe]   the plugin: one static, CGO-free binary
#
# `go` as the directory name is required, not cosmetic: `core/src/daemon/
# manifest.rs` enforces that a manifest's `language` equals its containing
# directory's name (the same rule plugins/go/plugin.toml's own header
# documents, and plugins/typescript/plugin.toml's before it).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PLUGIN_DIR="$REPO_ROOT/plugins/go"
GO_BIN="${GO_BIN:-go}"

# Kept in step with `SUPPORTED_TARGETS` in scripts/build-targets.sh.
declare -a SUPPORTED_TARGETS=(
	x86_64-apple-darwin
	aarch64-apple-darwin
	x86_64-unknown-linux-gnu
	x86_64-pc-windows-msvc
)

die() {
	echo "bundle-go-plugin: $*" >&2
	exit 1
}

log() {
	echo "==> $*"
}

host_triple() {
	if command -v rustc >/dev/null 2>&1; then
		rustc -vV | awk '/^host: / { print $2 }'
		return
	fi
	# rustc is the authority, but this script is useful without a Rust
	# toolchain, so fall back to uname for the four targets that matter.
	case "$(uname -s)/$(uname -m)" in
	Darwin/arm64) echo "aarch64-apple-darwin" ;;
	Darwin/x86_64) echo "x86_64-apple-darwin" ;;
	Linux/x86_64) echo "x86_64-unknown-linux-gnu" ;;
	MINGW* | MSYS* | CYGWIN*) echo "x86_64-pc-windows-msvc" ;;
	*) die "cannot determine the host target triple; install rustc or pass a target" ;;
	esac
}

# Rust target triple -> Go's own GOOS vocabulary.
goos_for() {
	case "$1" in
	*-apple-darwin) echo "darwin" ;;
	*-unknown-linux-gnu) echo "linux" ;;
	*-pc-windows-msvc) echo "windows" ;;
	*) die "no known GOOS for target: $1" ;;
	esac
}

# Rust target triple -> Go's own GOARCH vocabulary.
goarch_for() {
	case "$1" in
	x86_64-*) echo "amd64" ;;
	aarch64-*) echo "arm64" ;;
	*) die "no known GOARCH for target: $1" ;;
	esac
}

plugin_exe_name_for() {
	case "$1" in
	*-windows-*) echo "g-mesh-plugin-go.exe" ;;
	*) echo "g-mesh-plugin-go" ;;
	esac
}

main() {
	local target="${1:-}" dest="${2:-$REPO_ROOT/dist/plugins}"
	[ -n "$target" ] || target="$(host_triple)"

	printf '%s\n' "${SUPPORTED_TARGETS[@]}" | grep -qx "$target" ||
		die "unsupported target: $target (see scripts/build-targets.sh --list)"

	command -v "$GO_BIN" >/dev/null 2>&1 || die "$GO_BIN is required to build the Go plugin"

	local goos goarch exe_name stage
	goos="$(goos_for "$target")"
	goarch="$(goarch_for "$target")"
	exe_name="$(plugin_exe_name_for "$target")"
	stage="$dest/go"

	log "cross-compiling the Go plugin for $target (GOOS=$goos GOARCH=$goarch CGO_ENABLED=0) with $("$GO_BIN" version)"

	rm -rf "$stage"
	mkdir -p "$stage"

	(cd "$PLUGIN_DIR" && CGO_ENABLED=0 GOOS="$goos" GOARCH="$goarch" "$GO_BIN" build -o "$stage/$exe_name" .)
	[ -f "$stage/$exe_name" ] || die "expected binary not found: $stage/$exe_name"
	chmod +x "$stage/$exe_name" 2>/dev/null || true

	# See the header comment above ("THE WINDOWS GAP THIS SCRIPT CLOSES") for
	# why the installed manifest's command is rewritten instead of reused
	# as-is.
	log "generating $stage/plugin.toml for $exe_name"
	local src_manifest="$PLUGIN_DIR/plugin.toml"
	local marker='command = "./g-mesh-plugin-go"'
	grep -qF "$marker" "$src_manifest" ||
		die "$src_manifest no longer contains '$marker' - update this script's substitution to match its new spelling"

	{
		echo "# Bundled Go plugin manifest, as installed. Generated by"
		echo "# scripts/bundle-go-plugin.sh from plugins/go/plugin.toml - edit that file,"
		echo "# not this one. Only the [plugin.spawn] command line differs from it,"
		echo "# rewritten to name $exe_name, the binary actually staged beside this"
		echo "# manifest for $target (see GM-283 in that script for why)."
		echo "#"
		sed "s#$marker#command = \"./$exe_name\"#" "$src_manifest"
	} >"$stage/plugin.toml"

	grep -qF "command = \"./$exe_name\"" "$stage/plugin.toml" ||
		die "failed to rewrite the command line in the staged manifest"

	# Only meaningful when we built for the machine we are standing on - a
	# cross-built binary cannot be executed here (build-targets.sh's own
	# `--version` smoke test is native-only for the same reason).
	local host
	host="$(host_triple)"
	if [ "$target" = "$host" ]; then
		log "smoke test: handshake with no input"
		local handshake
		handshake="$(printf '' | "$stage/$exe_name" "$REPO_ROOT" 2>/dev/null || true)"
		case "$handshake" in
		*'"language":"go"'*) log "handshake ok" ;;
		*) die "the staged Go plugin did not produce a handshake (got: ${handshake:-<nothing>})" ;;
		esac
	else
		log "smoke test skipped: $target is not the host ($host)"
	fi

	log "staged: $stage ($(du -sh "$stage" | cut -f1))"
}

main "$@"
