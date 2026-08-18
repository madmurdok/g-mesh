#!/usr/bin/env bash
#
# Compiles the bundled JS/TS plugin into a self-contained executable and stages
# it, with everything it needs at runtime, as a `typescript/` plugin directory
# that core can discover next to its own binary.
#
#   scripts/bundle-plugin.sh                                   # host target, into dist/plugins
#   scripts/bundle-plugin.sh x86_64-apple-darwin /tmp/stage    # explicit target and destination
#
# `scripts/build-targets.sh` calls this while staging a release archive; it is
# runnable on its own so the bundle can be built and inspected without
# packaging a whole release.
#
# Environment:
#   NODE_BIN  the Node binary to build the runtime from (default: `node` on PATH)
#
# ---------------------------------------------------------------------------
# WHY NODE SEA AND NOT `bun build --compile`
#
# Both were tried, on this repo's actual plugin, before choosing. The task that
# scoped this work suggested either; the measurements decided it.
#
# `bun build --compile` fails on the plugin's native dependencies, in both
# available shapes (Bun 1.3.14, macOS x64):
#
#   - Inlined, the compiled binary aborts on the first parse:
#     `TypeError: Attempted to assign to readonly property` from
#     `initializeLanguageNodeClasses`, where node-tree-sitter's JS wrapper
#     assigns to `nodeSubclass.prototype.type`. (Bun runs the same code fine
#     *un*compiled - `bun app.js` parses TypeScript correctly - so this is the
#     bundler/compiler path specifically, not Bun's N-API support.)
#   - Marked `--external`, the compiled binary cannot find them at all:
#     module resolution inside a compiled Bun binary is rooted at the virtual
#     `/$bunfs/root/`, so `node_modules/` shipped beside the executable is
#     invisible - `Cannot find package 'tree-sitter-typescript'`.
#
# Node's SEA has neither problem: `createRequire` anchored at the executable's
# directory loads the prebuilt `.node` addons off disk (see
# plugins/typescript/sea/native-require.cjs), and the runtime doing so is the
# same Node the plugin is developed and measured against, so nothing about the
# plugin's behavior changes by shipping it this way. Bun's genuine advantage -
# `--target=bun-linux-x64` cross-compiles from any host - buys nothing here:
# task 64 already builds every triple on its own native runner, because the
# Rust side has four C/C++ dependencies that make true cross-compilation the
# harder path.
#
# The cost of SEA is that the runtime it produces is a copy of the *host's*
# Node binary, so a bundle can only be built for the platform it is built on -
# hence the host check below.
# ---------------------------------------------------------------------------
#
# WHAT ENDS UP IN THE STAGED DIRECTORY
#
#   typescript/
#     plugin.toml                    discovery manifest; spawns the executable
#                                    directly, with no interpreter
#     g-mesh-plugin-typescript[.exe] the plugin: Node + the whole compiled
#                                    bundle, in one file
#     node_modules/                  the three tree-sitter packages, pruned to
#                                    this target's prebuilt `.node` addons
#
# `typescript` as the directory name is required, not cosmetic:
# `core/src/daemon/manifest.rs` enforces that a manifest's `language` equals its
# containing directory's name.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PLUGIN_DIR="$REPO_ROOT/plugins/typescript"
NODE_BIN="${NODE_BIN:-node}"

# Kept in step with `SUPPORTED_TARGETS` in scripts/build-targets.sh.
declare -a SUPPORTED_TARGETS=(
	x86_64-apple-darwin
	aarch64-apple-darwin
	x86_64-unknown-linux-gnu
	x86_64-pc-windows-msvc
)

# The packages that cannot be bundled because they load a `.node` addon - see
# plugins/typescript/sea/native-require.cjs.
declare -a NATIVE_PACKAGES=(
	tree-sitter
	tree-sitter-javascript
	tree-sitter-typescript
)

die() {
	echo "bundle-plugin: $*" >&2
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

# `node-gyp-build` looks for `prebuilds/<platform>-<arch>/`, in Node's own
# vocabulary rather than Rust's - this is the translation between the two.
prebuild_dir_for() {
	case "$1" in
	x86_64-apple-darwin) echo "darwin-x64" ;;
	aarch64-apple-darwin) echo "darwin-arm64" ;;
	x86_64-unknown-linux-gnu) echo "linux-x64" ;;
	x86_64-pc-windows-msvc) echo "win32-x64" ;;
	*) die "no known prebuild directory for $1" ;;
	esac
}

plugin_exe_name_for() {
	case "$1" in
	*-windows-*) echo "g-mesh-plugin-typescript.exe" ;;
	*) echo "g-mesh-plugin-typescript" ;;
	esac
}

# Node's SEA blob is injected into a copy of the Node binary, and the injection
# invalidates whatever signature that binary carried. macOS refuses to run a
# Mach-O whose signature no longer matches, so the signature is stripped before
# and an ad-hoc one applied after. Nothing to do on Linux; on Windows the
# Authenticode signature is simply left invalid, which Windows tolerates for an
# unsigned-by-us binary (signing releases is its own, separate concern).
strip_signature() {
	case "$1" in
	*.exe) ;;
	*)
		if command -v codesign >/dev/null 2>&1; then
			codesign --remove-signature "$1" 2>/dev/null || true
		fi
		;;
	esac
}

sign_adhoc() {
	case "$1" in
	*.exe) ;;
	*)
		if command -v codesign >/dev/null 2>&1; then
			codesign --sign - "$1" || die "failed to ad-hoc sign $1"
		fi
		;;
	esac
}

# Everything a prebuilt addon package needs at runtime is JS, JSON and the one
# `.node` binary for this platform. The C sources, the grammar `.wasm` files and
# the five other platforms' prebuilds are what make these packages 45MB on disk,
# and none of it is loaded by anything (`tree-sitter-typescript` alone drops
# from 37MB to under 3MB). Pruned by removing what is not needed rather than by
# copying a hand-listed allowlist: a missed file then costs size, not a runtime
# failure that only shows up on one platform.
prune_native_package() {
	local pkg_dir="$1" keep_prebuild="$2" entry

	find "$pkg_dir" -type f \
		\( -name '*.c' -o -name '*.h' -o -name '*.cc' -o -name '*.cpp' -o -name '*.hpp' \
		-o -name '*.wasm' -o -name '*.gyp' -o -name '*.md' \) -delete
	rm -rf "$pkg_dir/queries" "$pkg_dir/vendor"

	if [ -d "$pkg_dir/prebuilds" ]; then
		for entry in "$pkg_dir/prebuilds"/*; do
			[ -e "$entry" ] || continue
			[ "$(basename "$entry")" = "$keep_prebuild" ] || rm -rf "$entry"
		done
		[ -d "$pkg_dir/prebuilds/$keep_prebuild" ] ||
			die "$(basename "$pkg_dir") ships no prebuilt addon for $keep_prebuild"
	fi
}

# Builds the plugin's whole TypeScript source into one CommonJS file, with the
# three native packages left as bare `require`s pointed at the on-disk loader.
#
# The input is `dist/`, not `src/`: `tsc` has already type-checked and compiled
# it (that is `npm run build`, which core's own build.rs runs), so bundling the
# compiled output means the executable contains exactly the code a dev checkout
# runs, rather than a second compilation of the same sources with a different
# compiler's semantics.
bundle_javascript() {
	local out="$1"
	local alias_args=()
	local pkg

	for pkg in "${NATIVE_PACKAGES[@]}"; do
		alias_args+=("--alias:$pkg=$PLUGIN_DIR/sea/$pkg.cjs")
	done

	(cd "$PLUGIN_DIR" && npx --no-install esbuild "dist/src/index.js" \
		--bundle \
		--platform=node \
		--format=cjs \
		--target=node20 \
		--define:__G_MESH_SELF_CONTAINED__=true \
		"${alias_args[@]}" \
		--outfile="$out")
}

main() {
	local target="${1:-}" dest="${2:-$REPO_ROOT/dist/plugins}"
	local host
	host="$(host_triple)"
	[ -n "$target" ] || target="$host"

	printf '%s\n' "${SUPPORTED_TARGETS[@]}" | grep -qx "$target" ||
		die "unsupported target: $target (see scripts/build-targets.sh --list)"

	# A SEA is a copy of the Node binary this script is running, so it can only
	# ever be a runtime for the platform it was built on. Failing loudly beats
	# staging a plugin that cannot execute on the machine the archive is for.
	[ "$target" = "$host" ] ||
		die "cannot bundle the plugin for $target on a $host host: a single-executable build embeds this machine's own Node runtime. Build each target on its own runner (see .github/workflows/release.yml)."

	command -v "$NODE_BIN" >/dev/null 2>&1 || die "$NODE_BIN is required to build the plugin runtime"
	command -v npm >/dev/null 2>&1 || die "npm is required"

	local node_major
	node_major="$("$NODE_BIN" -p 'process.versions.node.split(".")[0]')"
	[ "$node_major" -ge 20 ] ||
		die "Node 20+ is required to build a single-executable application (found $("$NODE_BIN" --version))"

	local exe_name prebuild stage
	exe_name="$(plugin_exe_name_for "$target")"
	prebuild="$(prebuild_dir_for "$target")"
	stage="$dest/typescript"

	log "bundling the JS/TS plugin for $target ($prebuild) with $("$NODE_BIN" --version)"

	# devDependencies included: esbuild and postject are what this script runs.
	log "installing plugin dependencies"
	(cd "$PLUGIN_DIR" && npm install --no-audit --no-fund --loglevel=error)

# Converts a shell path to one node itself can resolve.
#
# Git Bash on Windows hands the shell POSIX paths - `/tmp/tmp.XXXX`, or
# `/d/a/...` for `D:\a\...` - and every program it *launches* gets them
# translated automatically. Nothing translates a path that travels as data:
# a string inside sea-config.json, or inside a `require()` in `node -p`.
# Those reach node verbatim and fail as "Cannot read main script" or
# MODULE_NOT_FOUND, both of which name a path that looks perfectly valid.
#
# `cygpath -m` yields `C:/Users/...`: a real Windows path that keeps forward
# slashes, so it is also valid unescaped inside JSON. Off Windows there is no
# cygpath and the path is returned unchanged.
native_path() {
	if command -v cygpath >/dev/null 2>&1; then
		cygpath -m "$1"
	else
		printf '%s' "$1"
	fi
}

	log "compiling the plugin (tsc)"
	(cd "$PLUGIN_DIR" && npm run --silent build)

	local work work_native
	work="$(mktemp -d)"
	work_native="$(native_path "$work")"
	# shellcheck disable=SC2064 # $work is fixed at trap time on purpose.
	trap "rm -rf '$work'" EXIT

	log "bundling to a single CommonJS file (esbuild)"
	bundle_javascript "$work/plugin.cjs"

	# `disableExperimentalSEAWarning` keeps Node's "this is experimental"
	# banner off the plugin's stderr, which core forwards to its own.
	cat >"$work/sea-config.json" <<EOF
{
  "main": "$work_native/plugin.cjs",
  "output": "$work_native/sea-prep.blob",
  "disableExperimentalSEAWarning": true
}
EOF

	log "generating the SEA blob"
	"$NODE_BIN" --experimental-sea-config "$work/sea-config.json" >/dev/null

	rm -rf "$stage"
	mkdir -p "$stage"

	log "building the executable from this machine's Node runtime"
	local exe="$stage/$exe_name"
	cp "$(command -v "$NODE_BIN")" "$exe"
	chmod +w "$exe"
	strip_signature "$exe"
	(cd "$PLUGIN_DIR" && npx --no-install postject "$exe" NODE_SEA_BLOB "$work/sea-prep.blob" \
		--sentinel-fuse NODE_SEA_FUSE_fce680ab2cc467b6e072b8b5df1996b2 \
		--macho-segment-name NODE_SEA >/dev/null)
	sign_adhoc "$exe"
	chmod +x "$exe"

	log "staging native addons for $prebuild"
	local pkg
	for pkg in "${NATIVE_PACKAGES[@]}" node-gyp-build; do
		[ -d "$PLUGIN_DIR/node_modules/$pkg" ] || die "missing dependency: $pkg (run npm install)"
		mkdir -p "$stage/node_modules/$pkg"
		cp -R "$PLUGIN_DIR/node_modules/$pkg/." "$stage/node_modules/$pkg/"
		prune_native_package "$stage/node_modules/$pkg" "$prebuild"
	done

	# The executable is a copy of the Node binary, so the archive redistributes
	# Node itself and has to carry Node's license notice with it. Official
	# distributions (and Homebrew) keep it one level above `bin/`. Best-effort
	# rather than fatal: a Node built without it should not block a build, but
	# it must be loud, because the omission is a licensing problem and not a
	# cosmetic one. The tree-sitter packages' own LICENSE files survive
	# `prune_native_package` untouched.
	# Resolved through realpath, not `command -v` alone: a packaged Node is
	# usually a symlink into a versioned prefix (Homebrew, nvm, setup-node), and
	# the LICENSE lives beside the *real* `bin/`, not beside the symlink.
	local node_license real_node
	real_node="$("$NODE_BIN" -p 'require("node:fs").realpathSync(process.execPath)')"
	node_license="$(cd "$(dirname "$real_node")/.." && pwd)/LICENSE"
	if [ -f "$node_license" ]; then
		cp "$node_license" "$stage/LICENSE-nodejs"
	else
		echo "bundle-plugin: WARNING: no Node.js LICENSE found at $node_license - the archive redistributes the Node runtime and must ship its notice" >&2
	fi

	# The installed manifest, which is deliberately not the repo's own
	# plugins/typescript/plugin.toml: that one spawns `node dist/src/index.js`,
	# which is right for a checkout and impossible for an install. A relative
	# `command` is resolved against this manifest's own directory by
	# `core::daemon::manifest::read_manifest`, so the plugin does not need to
	# know where the archive was unpacked.
	local plugin_version
	plugin_version="$("$NODE_BIN" -p "require('$(native_path "$PLUGIN_DIR")/package.json').version")"
	cat >"$stage/plugin.toml" <<EOF
# Bundled JS/TS plugin, as installed. Generated by scripts/bundle-plugin.sh -
# edit that script, not this file.
#
# Unlike the manifest in the repo (plugins/typescript/plugin.toml), this one
# names no interpreter: the command below is a single-executable application
# that carries its own Node runtime, so an install needs no Node.js of its own.

[plugin]
language = "typescript"
protocol_version = 1
plugin_version = "$plugin_version"

[plugin.spawn]
command = "./$exe_name"
args = []

[plugin.languages]
extensions = [".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"]
EOF

	# A bundle that cannot even introduce itself is not worth packaging. Run
	# with a sanitised PATH holding no Node at all, which is the whole claim
	# this task makes: closing stdin immediately is how core ends a plugin, so
	# a healthy one answers with its handshake and exits 0.
	local handshake
	case "$target" in
	*-windows-*)
		# Not stripped down to a sanitised PATH here: on Windows the loader
		# resolves the executable's own system DLLs through it, so emptying it
		# would test the loader rather than the bundle. The Node-less claim is
		# proven on the platforms where it can be tested honestly.
		log "smoke test: handshake"
		handshake="$(printf '' | "$exe" "$REPO_ROOT" 2>/dev/null || true)"
		;;
	*)
		log "smoke test: handshake with no node on PATH"
		handshake="$(printf '' | env -i PATH="/usr/bin:/bin" HOME="$HOME" "$exe" "$REPO_ROOT" 2>/dev/null || true)"
		;;
	esac
	case "$handshake" in
	*'"language":"typescript"'*) log "handshake ok" ;;
	*) die "the staged plugin did not produce a handshake (got: ${handshake:-<nothing>})" ;;
	esac

	log "staged: $stage ($(du -sh "$stage" | cut -f1))"
}

main "$@"
