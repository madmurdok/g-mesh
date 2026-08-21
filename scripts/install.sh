#!/bin/sh
#
# Installs a released g-mesh: detects the platform, downloads the matching
# release archive from GitHub Releases, verifies its SHA-256 before unpacking,
# and puts core *and* its bundled plugin on disk together.
#
#   curl -fsSL https://raw.githubusercontent.com/madmurdok/g-mesh/main/scripts/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- --version 2.7.0
#   scripts/install.sh --install-dir ~/opt/g-mesh   # from a checkout
#
# This is the only script in scripts/ that is not bash. It is meant to be
# piped into whatever `/bin/sh` the machine has (dash on Debian/Ubuntu, ash on
# Alpine, bash on macOS), so it stays POSIX: no arrays, no `[[`, no
# `pipefail`. Everything else here - the `die`/`log` helpers, `set -eu`,
# refusing loudly instead of half-doing something - is deliberately the same
# shape as scripts/cut-release.sh and scripts/build-targets.sh.
#
# ---------------------------------------------------------------------------
# WHAT GETS INSTALLED, AND WHY IT IS A DIRECTORY AND NOT ONE FILE
#
# A release archive is a complete install, not a binary:
#
#   g-mesh                       the core binary
#   plugins/typescript/          the JS/TS plugin (its own embedded Node
#                                runtime) and the plugin.toml core discovers
#                                it through
#   LICENSE, LICENSE-MIT, LICENSE-APACHE, README.md
#
# Core cannot index anything without a plugin, and it finds one by looking for
# `plugins/` *next to the executable that is running*
# (`daemon::manifest::installed_bundled_root`, which is
# `std::env::current_exe()` + `/plugins`). So the two halves have to land in
# one directory, and that directory - not a copy of the binary - is what goes
# on `PATH`.
#
# That also rules out the usual `ln -s <install>/g-mesh /usr/local/bin/g-mesh`
# convenience: `current_exe()` resolves symlinks on Linux (`/proc/self/exe`)
# but NOT on macOS, where it returns the path the process was invoked through.
# A symlinked g-mesh on macOS would look for its plugin in `/usr/local/bin/
# plugins/`, find nothing, and fail to index - measured, not assumed. This
# script therefore never creates a symlink; it prints the one `PATH` line to
# add instead, and touches no shell rc file on its own.
#
# Default location: ~/.g-mesh/bin - inside the directory g-mesh already owns
# (config, project indexes and the embedding model live under ~/.g-mesh), so
# uninstalling is `rm -rf ~/.g-mesh/bin` and your settings survive it. It does
# not collide with the *user* plugin root `~/.g-mesh/plugins/`: the bundled
# plugin sits at `~/.g-mesh/bin/plugins/`, and a plugin you install yourself
# still outranks it.
#
# ---------------------------------------------------------------------------
# CHECKSUMS
#
# A release publishes `<asset>.sha256` beside every archive and one combined
# `SHA256SUMS`. This script fetches the per-asset file: it already knows the
# single archive it wants, so that is one small request instead of parsing a
# four-target list, and it is exactly the split
# .github/workflows/release.yml's header describes (SHA256SUMS is for a human
# running `sha256sum -c` by hand). The hash is computed with the same fallback
# chain build-targets.sh uses - sha256sum, else shasum -a 256 - plus openssl
# as a last resort, and compared before a single byte is unpacked. A mismatch
# aborts with both hashes printed and nothing installed.
#
# ---------------------------------------------------------------------------
# VERSIONS, AND THE "NOTHING IS PUBLISHED YET" CASE
#
# Asset names embed the version (`g-mesh-v<version>-<target>.tar.gz`), so
# `/releases/latest/download/...` is unusable here - you cannot name the file
# without first knowing the version. The version therefore comes from the
# `releases/latest` API, or from `--version`/`G_MESH_VERSION` to pin one.
#
# Releases are created as DRAFTS and stay invisible until a human publishes
# them (.github/workflows/release.yml, task #67). Until that happens the API
# has no latest release and every asset URL 404s. That is not an error worth a
# bare "404" - the script says what state the repository is in and what to do
# about it (see `no_published_release`).
#
# ---------------------------------------------------------------------------
# WINDOWS IS OUT OF SCOPE FOR THIS SCRIPT
#
# The Windows target ships a `.zip`, and a POSIX shell has no portable
# unzipper. Running this under Git Bash / MSYS would mean pretending; instead
# it refuses with the three manual steps that actually work. Native Windows
# support would be a separate `install.ps1`, not a branch of this file.
#
# ---------------------------------------------------------------------------
# TESTING IT WITHOUT A RELEASE
#
# Every URL is injectable, which is how this script was tested against a local
# fixture (a real archive from `scripts/build-targets.sh`, served over
# 127.0.0.1) before any release existed:
#
#   G_MESH_DOWNLOAD_BASE=http://127.0.0.1:8000 \
#   G_MESH_INSTALL_DIR=/tmp/g-mesh-test \
#     sh scripts/install.sh --version 2.7.0
#
# Environment:
#   G_MESH_VERSION        version to install (same name build-targets.sh uses)
#   G_MESH_INSTALL_DIR    where to install (default: ~/.g-mesh/bin)
#   G_MESH_TARGET         override the detected Rust target triple
#   G_MESH_REPO           owner/repo (default: madmurdok/g-mesh)
#   G_MESH_DOWNLOAD_BASE  base for <version-tag>/<asset> URLs
#   G_MESH_LATEST_API     the releases/latest endpoint
#   GITHUB_TOKEN          if set, authenticates the API call (rate limits)
# ---------------------------------------------------------------------------

set -eu

REPO="${G_MESH_REPO:-madmurdok/g-mesh}"
DOWNLOAD_BASE="${G_MESH_DOWNLOAD_BASE:-https://github.com/$REPO/releases/download}"
LATEST_API="${G_MESH_LATEST_API:-https://api.github.com/repos/$REPO/releases/latest}"
INSTALL_DIR="${G_MESH_INSTALL_DIR:-${HOME:-}/.g-mesh/bin}"
VERSION="${G_MESH_VERSION:-}"
TARGET="${G_MESH_TARGET:-}"
FORCE=0

# The three platforms this script can install. The fourth supported target,
# x86_64-pc-windows-msvc, is deliberately not here - see the header.
SUPPORTED_TARGETS='x86_64-apple-darwin aarch64-apple-darwin x86_64-unknown-linux-gnu'

die() {
	echo "install: $*" >&2
	exit 1
}

log() {
	echo "==> $*"
}

have() {
	command -v "$1" >/dev/null 2>&1
}

usage() {
	cat <<'EOF'
usage: install.sh [--version X.Y.Z] [--install-dir DIR] [--target TRIPLE] [--force]

  --version X.Y.Z    install this release instead of the latest published one
  --install-dir DIR  install root (default: ~/.g-mesh/bin); the binary and its
                     plugins/ directory both live here, and this is the
                     directory that goes on PATH
  --target TRIPLE    override platform detection (advanced/testing)
  --force            replace a non-empty install directory that does not look
                     like an existing g-mesh install
  -h, --help         this message

Installs macOS (Intel/Apple Silicon) and x86_64 Linux (glibc) builds.
Windows is not supported by this script: that target ships a .zip - download
it from the releases page and unpack it, keeping g-mesh.exe and plugins/
together in one directory.
EOF
}

# ---------------------------------------------------------------------------
# Fetching. Both helpers fail (non-zero) rather than dying, so each caller can
# say what a failure means there: a missing asset, an unreachable network and
# an unpublished release need different advice, and "curl: (22)" is none of
# them.

# HTTPS is pinned only when the URL is already https, so that
# G_MESH_DOWNLOAD_BASE can point at a local fixture server during testing
# without the transport flags fighting it.
download() {
	_url="$1"
	_dest="$2"
	if have curl; then
		case "$_url" in
		https://*) curl --proto '=https' --tlsv1.2 -fsSL -o "$_dest" "$_url" ;;
		*) curl -fsSL -o "$_dest" "$_url" ;;
		esac
	elif have wget; then
		wget -q -O "$_dest" "$_url"
	else
		die "neither curl nor wget is available - one of them is needed to download anything"
	fi
}

api_get() {
	_url="$1"
	if have curl; then
		if [ -n "${GITHUB_TOKEN:-}" ]; then
			curl -fsSL -H 'Accept: application/vnd.github+json' \
				-H "Authorization: Bearer $GITHUB_TOKEN" "$_url"
		else
			curl -fsSL -H 'Accept: application/vnd.github+json' "$_url"
		fi
	elif have wget; then
		if [ -n "${GITHUB_TOKEN:-}" ]; then
			wget -q -O - --header 'Accept: application/vnd.github+json' \
				--header "Authorization: Bearer $GITHUB_TOKEN" "$_url"
		else
			wget -q -O - --header 'Accept: application/vnd.github+json' "$_url"
		fi
	else
		die "neither curl nor wget is available - one of them is needed to download anything"
	fi
}

# ---------------------------------------------------------------------------
# Platform detection

windows_not_supported() {
	cat >&2 <<EOF
install: this script cannot install g-mesh on Windows.

The Windows build ships as a .zip, which a POSIX shell has no portable way to
unpack, so rather than half-installing it this script stops here. Install it
by hand instead - it is three steps:

  1. Download g-mesh-v<version>-x86_64-pc-windows-msvc.zip from
     https://github.com/$REPO/releases
  2. Unpack it somewhere permanent, keeping g-mesh.exe and the plugins\\
     directory beside each other. That layout is not cosmetic: g-mesh finds
     its language plugin next to the running executable, and moving g-mesh.exe
     out on its own gives you a binary that cannot index anything.
  3. Add that directory to your PATH.
EOF
	exit 1
}

# Prints the Rust target triple for this machine, or dies explaining which
# platforms have builds.
detect_target() {
	_os="$(uname -s 2>/dev/null || echo unknown)"
	_arch="$(uname -m 2>/dev/null || echo unknown)"

	case "$_os" in
	Darwin)
		case "$_arch" in
		arm64 | aarch64) echo 'aarch64-apple-darwin' ;;
		x86_64)
			# A shell running under Rosetta on Apple Silicon reports x86_64.
			# Installing the Intel build would work but would be slower than
			# the native one that also exists, so ask the kernel instead of
			# trusting uname here.
			if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = "1" ]; then
				echo 'aarch64-apple-darwin'
			else
				echo 'x86_64-apple-darwin'
			fi
			;;
		*) die "unsupported macOS architecture: $_arch (builds exist for x86_64 and arm64)" ;;
		esac
		;;
	Linux)
		case "$_arch" in
		x86_64 | amd64) ;;
		aarch64 | arm64)
			die "no aarch64 Linux build is published - g-mesh releases cover $SUPPORTED_TARGETS. Build from source: https://github.com/$REPO#build"
			;;
		*)
			die "unsupported Linux architecture: $_arch - g-mesh releases cover $SUPPORTED_TARGETS. Build from source: https://github.com/$REPO#build"
			;;
		esac
		# The Linux artifact is a *-gnu build. On musl (Alpine) it would
		# install fine and then fail to exec, which is a far worse failure
		# than refusing now.
		if [ -f /etc/alpine-release ] || { ldd --version 2>&1 || true; } | grep -qi musl; then
			die "this looks like a musl system (Alpine); only a glibc (*-unknown-linux-gnu) Linux build is published. Build from source: https://github.com/$REPO#build"
		fi
		echo 'x86_64-unknown-linux-gnu'
		;;
	MINGW* | MSYS* | CYGWIN* | Windows_NT)
		windows_not_supported
		;;
	*)
		die "unsupported operating system: $_os - g-mesh releases cover $SUPPORTED_TARGETS"
		;;
	esac
}

# ---------------------------------------------------------------------------
# Version resolution

# Every way of failing to learn the latest version lands here, because the
# call that establishes it either answers with a tag or it does not - see
# `resolve_latest_version`. So this cannot claim to know which happened, and
# ordering the causes by likelihood is the most honest thing it can do.
#
# That order changed once a release existed. While the repository had none,
# "a draft is waiting for someone to press Publish" was the expected state and
# led. Now that v2.8.0 is out, a caller who reaches this message far more
# likely hit the unauthenticated API's 60-requests-per-hour limit, or has no
# route to it at all - both of which the API reports in a way this script
# cannot tell apart from "no release".
no_published_release() {
	cat >&2 <<EOF
install: could not work out which release to install.

This asked for the latest published release and got no answer:

  $LATEST_API

Most likely, in this order:
  - The GitHub API rate-limited you. Unauthenticated calls get 60 per hour
    per IP, and this looks identical to "no release exists". Set GITHUB_TOKEN
    to raise the limit, or wait an hour.
  - You have no route to api.github.com - a proxy, a firewall, or no network.
  - There genuinely is no published release. Releases here are built as
    drafts and stay invisible, with their download URLs 404ing, until a human
    publishes one, so this is the expected state between a build finishing
    and someone pressing Publish.

What you can do:
  - Check https://github.com/$REPO/releases to see what is published.
  - Install a specific version, skipping the API call entirely:
      curl -fsSL https://raw.githubusercontent.com/$REPO/main/scripts/install.sh | sh -s -- --version X.Y.Z
  - Build from source meanwhile: https://github.com/$REPO#build
EOF
	exit 1
}

resolve_latest_version() {
	_json="$(api_get "$LATEST_API" 2>/dev/null)" || no_published_release
	_tag="$(printf '%s\n' "$_json" |
		sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
		head -n 1)"
	[ -n "$_tag" ] || no_published_release
	printf '%s\n' "${_tag#v}"
}

# ---------------------------------------------------------------------------
# Checksums - the same fallback chain as build-targets.sh's sha256_of, plus
# openssl for the minimal images that have neither of the first two. Prints
# the bare lowercase hex digest, without the filename the tools append.
sha256_of() {
	if have sha256sum; then
		sha256sum "$1" | awk '{ print $1 }'
	elif have shasum; then
		shasum -a 256 "$1" | awk '{ print $1 }'
	elif have openssl; then
		openssl dgst -sha256 "$1" | awk '{ print $NF }'
	else
		die "no sha256sum, shasum or openssl available - cannot verify the download, and installing an unverified binary is not something this script will do"
	fi
}

# Installing replaces the whole install directory - that is how an old
# install's stale plugin files disappear instead of lingering beside the new
# ones. It is also how a mistyped --install-dir would eat someone's ~/bin, so
# an existing g-mesh install is replaced silently, an empty directory is used,
# and anything else is refused. Checked before the download rather than after
# it: a typo should cost a second, not 25 MB.
check_install_dir() {
	[ -e "$INSTALL_DIR" ] || return 0
	[ -d "$INSTALL_DIR" ] || die "$INSTALL_DIR exists and is not a directory"
	[ ! -f "$INSTALL_DIR/g-mesh" ] || return 0
	[ -n "$(ls -A "$INSTALL_DIR" 2>/dev/null)" ] || return 0
	[ "$FORCE" -eq 1 ] ||
		die "$INSTALL_DIR is not empty and does not look like a g-mesh install (no g-mesh binary in it). Installing would replace its whole contents - pass a different --install-dir, or --force if you meant this one."
}

# ---------------------------------------------------------------------------

main() {
	while [ $# -gt 0 ]; do
		case "$1" in
		--version)
			[ $# -ge 2 ] || die "--version needs a version, e.g. --version 2.7.0"
			VERSION="$2"
			shift
			;;
		--version=*) VERSION="${1#--version=}" ;;
		--install-dir)
			[ $# -ge 2 ] || die "--install-dir needs a path"
			INSTALL_DIR="$2"
			shift
			;;
		--install-dir=*) INSTALL_DIR="${1#--install-dir=}" ;;
		--target)
			[ $# -ge 2 ] || die "--target needs a target triple"
			TARGET="$2"
			shift
			;;
		--target=*) TARGET="${1#--target=}" ;;
		--force) FORCE=1 ;;
		-h | --help)
			usage
			return 0
			;;
		*) die "unknown argument: $1 (try --help)" ;;
		esac
		shift
	done

	[ -n "$INSTALL_DIR" ] || die "no install directory: pass --install-dir, or set HOME"
	case "$INSTALL_DIR" in
	/*) ;;
	*) INSTALL_DIR="$(pwd)/$INSTALL_DIR" ;;
	esac

	have tar || die "tar is required to unpack the release archive"
	check_install_dir

	if [ -z "$TARGET" ]; then
		TARGET="$(detect_target)" || exit 1
	fi
	case "$TARGET" in
	*-windows-*) windows_not_supported ;;
	esac

	if [ -n "$VERSION" ]; then
		VERSION="${VERSION#v}"
		printf '%s\n' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' ||
			die "malformed version: '$VERSION' (expected X.Y.Z, e.g. 2.7.0)"
	else
		log "resolving the latest published release"
		VERSION="$(resolve_latest_version)" || exit 1
	fi

	_stem="g-mesh-v$VERSION-$TARGET"
	_asset="$_stem.tar.gz"
	_url="$DOWNLOAD_BASE/v$VERSION/$_asset"

	log "g-mesh $VERSION for $TARGET -> $INSTALL_DIR"

	_work="$(mktemp -d "${TMPDIR:-/tmp}/g-mesh-install.XXXXXX")" ||
		die "could not create a temporary directory"
	# shellcheck disable=SC2064 # $_work is fixed at trap time on purpose.
	trap "rm -rf '$_work'" EXIT INT TERM HUP

	log "downloading $_asset"
	download "$_url" "$_work/$_asset" || die "could not download $_url
The release may not be published yet, or may not include a build for $TARGET.
Check https://github.com/$REPO/releases"

	log "downloading its checksum"
	download "$_url.sha256" "$_work/$_asset.sha256" || die "could not download $_url.sha256
The archive downloaded but its checksum did not, so the download cannot be
verified - refusing to install unverified bytes."

	# The .sha256 file is `<hex>  <basename>`, written by build-targets.sh and
	# re-checked at publish time by prepare-release-assets.sh; only the digest
	# matters here, since we know which file we just fetched.
	_expected="$(awk '{ print $1 }' "$_work/$_asset.sha256" | tr '[:upper:]' '[:lower:]')"
	[ -n "$_expected" ] || die "the published checksum file for $_asset is empty or malformed - refusing to install unverified bytes"
	_actual="$(sha256_of "$_work/$_asset" | tr '[:upper:]' '[:lower:]')"
	if [ "$_expected" != "$_actual" ]; then
		die "checksum mismatch for $_asset - NOTHING was installed.
  expected: $_expected
  actual:   $_actual
The download is corrupt or has been tampered with. Retry; if it keeps
failing, report it at https://github.com/$REPO/issues rather than installing
this binary."
	fi
	log "checksum ok"

	log "unpacking"
	mkdir -p "$_work/unpack"
	tar -xzf "$_work/$_asset" -C "$_work/unpack" ||
		die "could not unpack $_asset (checksum matched, so this is a tar problem, not a corrupt download)"

	_stage="$_work/unpack/$_stem"
	[ -d "$_stage" ] || die "unexpected archive layout: $_asset does not contain a $_stem/ directory"
	[ -f "$_stage/g-mesh" ] || die "unexpected archive layout: no g-mesh binary inside $_asset"
	[ -f "$_stage/plugins/typescript/plugin.toml" ] ||
		die "unexpected archive layout: $_asset carries no plugins/typescript/plugin.toml. Core cannot index anything without the plugin, so this archive is not installable."
	chmod +x "$_stage/g-mesh" 2>/dev/null || true

	# Run it before installing it. `--version` proves the binary executes on
	# this machine at all, and `plugins list` proves it discovers the plugin
	# that travelled with it - the one failure mode that otherwise shows up
	# only later, as a daemon that refuses to start.
	log "verifying the downloaded binary runs"
	_reported="$("$_stage/g-mesh" --version 2>/dev/null)" ||
		die "the downloaded g-mesh does not run on this machine (target $TARGET) - nothing was installed"
	case "$_reported" in
	*"$VERSION"*) ;;
	*) die "version mismatch: the archive is named $VERSION but the binary reports '$_reported' - nothing was installed" ;;
	esac
	"$_stage/g-mesh" plugins list 2>/dev/null | grep -q typescript ||
		die "the downloaded g-mesh does not see the plugin that shipped with it - nothing was installed"

	log "installing into $INSTALL_DIR"
	mkdir -p "$(dirname "$INSTALL_DIR")" || die "could not create $(dirname "$INSTALL_DIR")"
	_new="$INSTALL_DIR.new-$$"
	_old="$INSTALL_DIR.old-$$"
	rm -rf "$_new" "$_old"
	mv "$_stage" "$_new" || die "could not stage the new install at $_new"
	if [ -d "$INSTALL_DIR" ]; then
		mv "$INSTALL_DIR" "$_old" || {
			rm -rf "$_new"
			die "could not move the existing install aside ($INSTALL_DIR) - nothing was changed"
		}
	fi
	if ! mv "$_new" "$INSTALL_DIR"; then
		# Put the previous install back rather than leaving the machine with
		# neither.
		if [ -d "$_old" ]; then
			mv "$_old" "$INSTALL_DIR"
		fi
		rm -rf "$_new"
		die "could not install into $INSTALL_DIR (permissions?) - the previous install was left in place"
	fi
	rm -rf "$_old"

	echo
	log "installed g-mesh $VERSION"
	echo "  binary:  $INSTALL_DIR/g-mesh"
	echo "  plugin:  $INSTALL_DIR/plugins/typescript/  (must stay beside the binary)"
	echo

	case ":$PATH:" in
	*":$INSTALL_DIR:"*)
		echo "$INSTALL_DIR is already on your PATH. Try:"
		echo
		echo "  g-mesh --version"
		;;
	*)
		_rc='your shell profile'
		# The tildes below are deliberate and must not become $HOME: this
		# string is printed for a human to read, as the trailing comment on
		# a sample `export PATH` line. "~/.zshrc" is how a person refers to
		# that file; expanding it to /Users/someone/.zshrc would make the
		# advice longer and no clearer, and this script never opens any of
		# these paths - it says outright that it does not edit rc files.
		# shellcheck disable=SC2088
		case "$(basename "${SHELL:-sh}")" in
		zsh) _rc="~/.zshrc" ;;
		bash) _rc="~/.bashrc (macOS: ~/.bash_profile)" ;;
		fish) _rc="~/.config/fish/config.fish - there, use: fish_add_path $INSTALL_DIR" ;;
		esac
		echo "Add it to your PATH - this script does not edit shell rc files:"
		echo
		echo "  export PATH=\"$INSTALL_DIR:\$PATH\"      # in $_rc"
		echo
		echo "Then: g-mesh --version"
		;;
	esac
	echo
	echo "Register it with Claude Code:"
	echo
	echo "  claude mcp add g-mesh -s user -- $INSTALL_DIR/g-mesh mcp-shim"
	echo
	echo "The seven structural tools work as-is. \`search_code\` additionally needs"
	echo "the embedding model: g-mesh model fetch"
	echo
	echo "To uninstall: rm -rf $INSTALL_DIR"
}

main "$@"
