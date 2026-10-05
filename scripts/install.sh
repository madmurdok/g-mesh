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
#   plugins/typescript/          the JS/TS plugin (a plain cargo binary,
#                                needing no Node.js runtime, GM-326) and the
#                                plugin.toml core discovers it through
#   plugins/go/                  the Go plugin (one static binary) and its
#                                own plugin.toml (GM-283)
#   plugins/rust/                the Rust plugin (a plain cargo binary,
#                                needing no runtime of its own) and its own
#                                plugin.toml (GM-288)
#   plugins/python/              the Python plugin (a plain cargo binary,
#                                needing no runtime of its own, and no Python
#                                interpreter on this machine for the
#                                structural tier it ships today - a future
#                                semantic tier would need pyright installed
#                                separately) and its own plugin.toml (GM-298)
#   LICENSE, LICENSE-MIT, LICENSE-APACHE, README.md
#
# Core cannot index a TypeScript project without the JS/TS plugin, and it
# finds both plugins by looking for `plugins/` *next to the executable that is
# running* (`daemon::manifest::installed_bundled_root`, which is
# `std::env::current_exe()` + `/plugins`). So every part of the install has to
# land in one directory, and that directory - not a copy of the binary - is
# what goes on `PATH`.
#
# That also rules out the usual `ln -s <install>/g-mesh /usr/local/bin/g-mesh`
# convenience: `current_exe()` resolves symlinks on Linux (`/proc/self/exe`)
# but NOT on macOS, where it returns the path the process was invoked through.
# A symlinked g-mesh on macOS would look for its plugin in `/usr/local/bin/
# plugins/`, find nothing, and fail to index - measured, not assumed. This
# script therefore never creates a symlink; it puts the install directory
# itself on `PATH` instead (next section).
#
# ---------------------------------------------------------------------------
# PATH: EDITED BY DEFAULT, RUSTUP-STYLE
#
# So that `g-mesh` is found right after installing, the script appends one
# marker-delimited block to the rc file of the shell in $SHELL:
#
#   zsh     ~/.zshrc
#   bash    ~/.bashrc on Linux; ~/.bash_profile on macOS (Terminal starts a
#           login shell there). If ~/.bash_profile does not exist yet but
#           ~/.bash_login or ~/.profile does, that file is used instead:
#           creating ~/.bash_profile would make bash stop reading them.
#   fish    ~/.config/fish/conf.d/g-mesh.fish ($XDG_CONFIG_HOME is honoured),
#           using fish_add_path
#   other   ~/.profile
#
# The block is delimited by `# >>> g-mesh installer >>>` and
# `# <<< g-mesh installer <<<`; deleting those lines and what is between them
# undoes it. Nothing is written when the install directory is already on
# PATH, or when the block is already in the file (so re-installing changes
# nothing). The file is only ever appended to, never rewritten, so a
# symlinked dotfile stays a symlink. If the append fails (a read-only rc
# file), the install still succeeds and the old advice is printed.
#
# Opt out with --no-modify-path or G_MESH_NO_MODIFY_PATH=1: then no file is
# touched and the script prints the `export PATH` line to add by hand.
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
# it refuses and points at `scripts/install.ps1` (GM-208), which does the same
# download-verify-unpack-advise sequence in PowerShell, the interpreter
# actually present on every target machine this script cannot serve.
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
#     sh scripts/install.sh --version 2.7.0 --no-modify-path
#
# scripts/test-install.sh does the same against a fake archive, in a
# throwaway HOME, to test the PATH editing above.
#
# Environment:
#   G_MESH_VERSION        version to install (same name build-targets.sh uses)
#   G_MESH_INSTALL_DIR    where to install (default: ~/.g-mesh/bin)
#   G_MESH_TARGET         override the detected Rust target triple
#   G_MESH_REPO           owner/repo (default: madmurdok/g-mesh)
#   G_MESH_DOWNLOAD_BASE  base for <version-tag>/<asset> URLs
#   G_MESH_LATEST_API     the releases/latest endpoint
#   G_MESH_NO_MODIFY_PATH set to anything but 0: leave shell rc files alone
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
case "${G_MESH_NO_MODIFY_PATH:-}" in
'' | 0) MODIFY_PATH=1 ;;
*) MODIFY_PATH=0 ;;
esac

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
                  [--no-modify-path]

  --version X.Y.Z    install this release instead of the latest published one
  --install-dir DIR  install root (default: ~/.g-mesh/bin); the binary and its
                     plugins/ directory both live here, and this is the
                     directory that goes on PATH
  --target TRIPLE    override platform detection (advanced/testing)
  --force            replace a non-empty install directory that does not look
                     like an existing g-mesh install
  --no-modify-path   do not add the install directory to PATH in your shell's
                     rc file (same as G_MESH_NO_MODIFY_PATH=1); print the line
                     to add by hand instead
  -h, --help         this message

Installs macOS (Intel/Apple Silicon) and x86_64 Linux (glibc 2.34+) builds.
Windows is not supported by this script: that target ships a .zip, which a
POSIX shell has no portable way to unpack. Use scripts/install.ps1 instead:

  irm https://raw.githubusercontent.com/madmurdok/g-mesh/main/scripts/install.ps1 | iex
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
unpack, so rather than half-installing it this script stops here. Use
scripts/install.ps1 instead - it is the same download/verify/unpack sequence,
in PowerShell:

  irm https://raw.githubusercontent.com/$REPO/main/scripts/install.ps1 | iex

Or by hand, if you would rather not run a script - it is three steps:

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
		# Same failure one step finer, and the same reason to refuse early.
		# The artifact needs glibc >= 2.34 (and GLIBCXX_3.4.29, which ships on
		# the same distros): GM-332 measured that by running it, not by
		# reading the linker. Below the floor it downloads, verifies, unpacks
		# and then dies at exec with `libc.so.6: version 'GLIBC_2.34' not
		# found` - a message that names no remedy and no cause.
		_libc=$({ getconf GNU_LIBC_VERSION 2>/dev/null || ldd --version 2>/dev/null | head -n 1 || true; } |
			tr ' ' '\n' | grep -E '^[0-9]+\.[0-9]+' | head -n 1 | sed 's/[^0-9.].*$//')
		case "$_libc" in
		# Anything we cannot parse is left alone on purpose: a wrong refusal
		# on an exotic-but-fine system is worse than the honest exec error.
		[0-9]*.[0-9]*)
			_libc_major=${_libc%%.*}
			_libc_minor=${_libc#*.}
			_libc_minor=${_libc_minor%%.*}
			if [ "$_libc_major" -lt 2 ] ||
				{ [ "$_libc_major" -eq 2 ] && [ "$_libc_minor" -lt 34 ]; }; then
				die "this system has glibc $_libc; the published Linux build needs glibc 2.34 or newer (it would install and then fail to start). Distros below the floor include Ubuntu 20.04, Debian 11, RHEL 8, Amazon Linux 2 and CentOS 7. Build from source: https://github.com/$REPO#build"
			fi
			;;
		esac
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
# PATH editing - see the header's PATH section for the policy.

PATH_BLOCK_START='# >>> g-mesh installer >>>'
PATH_BLOCK_END='# <<< g-mesh installer <<<'

# Prints the rc file for the shell in $SHELL. Bash on macOS is decided by the
# target being installed rather than by asking uname again: it is the same
# answer, and it lets scripts/test-install.sh exercise both cases on one host.
rc_file_for_shell() {
	case "$(basename "${SHELL:-sh}")" in
	zsh) echo "$HOME/.zshrc" ;;
	bash)
		case "$TARGET" in
		*-apple-darwin)
			# A login shell reads the first of these three that exists, so
			# creating ~/.bash_profile beside an existing ~/.profile would
			# silently stop bash from reading ~/.profile.
			for _f in .bash_profile .bash_login .profile; do
				if [ -e "$HOME/$_f" ]; then
					echo "$HOME/$_f"
					return 0
				fi
			done
			echo "$HOME/.bash_profile"
			;;
		*) echo "$HOME/.bashrc" ;;
		esac
		;;
	fish)
		case "${XDG_CONFIG_HOME:-}" in
		/*) echo "$XDG_CONFIG_HOME/fish/conf.d/g-mesh.fish" ;;
		*) echo "$HOME/.config/fish/conf.d/g-mesh.fish" ;;
		esac
		;;
	*) echo "$HOME/.profile" ;;
	esac
}

# The line that puts INSTALL_DIR on PATH, in the dialect of the file it goes
# into. Single-quoted, so nothing in the path is expanded when the rc file is
# sourced; modify_path refuses a path that itself contains a single quote.
path_line_for() {
	case "$1" in
	*.fish) echo "fish_add_path -g '$INSTALL_DIR'" ;;
	*) echo "export PATH='$INSTALL_DIR':\"\$PATH\"" ;;
	esac
}

# Sets PATH_RESULT to what happened, and PATH_RC to the rc file involved (if
# any):
#   onpath   INSTALL_DIR is already on PATH - nothing to do
#   optout   --no-modify-path / G_MESH_NO_MODIFY_PATH
#   unsafe   INSTALL_DIR contains a quote or a newline; not written
#   present  PATH_RC already has our block for INSTALL_DIR - left unchanged
#   stale    PATH_RC has our block, for another directory - left unchanged
#   added    the block was appended to PATH_RC
#   failed   PATH_RC could not be written
# Never dies: the install itself has already succeeded by the time this runs.
modify_path() {
	PATH_RC=''
	case ":$PATH:" in
	*":$INSTALL_DIR:"*)
		PATH_RESULT=onpath
		return 0
		;;
	esac
	if [ "$MODIFY_PATH" -ne 1 ]; then
		PATH_RESULT=optout
		return 0
	fi
	case "$INSTALL_DIR" in
	*"'"* | *'
'*)
		PATH_RESULT=unsafe
		return 0
		;;
	esac
	if [ -z "${HOME:-}" ]; then
		PATH_RESULT=failed
		return 0
	fi
	PATH_RC="$(rc_file_for_shell)"
	_line="$(path_line_for "$PATH_RC")"
	if [ -f "$PATH_RC" ] && grep -qF "$PATH_BLOCK_START" "$PATH_RC"; then
		if grep -qxF "$_line" "$PATH_RC"; then
			PATH_RESULT=present
		else
			PATH_RESULT=stale
		fi
		return 0
	fi
	if ! mkdir -p "$(dirname "$PATH_RC")" 2>/dev/null; then
		PATH_RESULT=failed
		return 0
	fi
	# Appended, never rewritten: a symlinked dotfile stays a symlink. A file
	# whose last line has no newline gets one first, so the marker does not
	# end up glued to the user's last line.
	_sep=''
	if [ -s "$PATH_RC" ]; then
		_sep='
'
		[ -z "$(tail -c 1 "$PATH_RC")" ] || _sep='

'
	fi
	if {
		printf '%s' "$_sep"
		echo "$PATH_BLOCK_START"
		echo "# Added by g-mesh's install.sh. Delete this block to undo it."
		echo "$_line"
		echo "$PATH_BLOCK_END"
	} 2>/dev/null >>"$PATH_RC"; then
		PATH_RESULT=added
	else
		PATH_RESULT=failed
	fi
}

# PATH_RC as a person would write it: ~/.zshrc rather than /Users/x/.zshrc.
rc_display() {
	case "$PATH_RC" in
	"$HOME"/*)
		# shellcheck disable=SC2088 # printed for a human, not expanded
		echo "~${PATH_RC#"$HOME"}"
		;;
	*) echo "$PATH_RC" ;;
	esac
}

# The manual advice: the line to add by hand, and where.
print_path_advice() {
	_rc='your shell profile'
	# The tildes below are deliberate and must not become $HOME: this
	# string is printed for a human to read, as the trailing comment on a
	# sample `export PATH` line.
	# shellcheck disable=SC2088
	case "$(basename "${SHELL:-sh}")" in
	zsh) _rc="~/.zshrc" ;;
	bash) _rc="~/.bashrc (macOS: ~/.bash_profile)" ;;
	fish) _rc="~/.config/fish/config.fish - there, use: fish_add_path $INSTALL_DIR" ;;
	esac
	echo "Add it to your PATH:"
	echo
	echo "  export PATH=\"$INSTALL_DIR:\$PATH\"      # in $_rc"
	echo
	echo "Then: g-mesh --version"
}

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
		--no-modify-path) MODIFY_PATH=0 ;;
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
		die "unexpected archive layout: $_asset carries no plugins/typescript/plugin.toml. Core cannot index a TypeScript project without it, so this archive is not installable."
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
	echo "  plugins: $INSTALL_DIR/plugins/  (must stay beside the binary)"
	echo

	modify_path
	case "$PATH_RESULT" in
	onpath)
		echo "$INSTALL_DIR is already on your PATH. Try:"
		echo
		echo "  g-mesh --version"
		;;
	added)
		echo "Added $INSTALL_DIR to your PATH in $(rc_display)."
		echo "Restart your shell (or open a new terminal), then: g-mesh --version"
		;;
	present)
		echo "$(rc_display) already puts $INSTALL_DIR on your PATH (left unchanged)."
		echo "Restart your shell (or open a new terminal), then: g-mesh --version"
		;;
	stale)
		echo "$(rc_display) already has a g-mesh PATH block, but for another directory;"
		echo "it was left alone. Change the line inside it to:"
		echo
		echo "  $(path_line_for "$PATH_RC")"
		echo
		echo "Then restart your shell and run: g-mesh --version"
		;;
	unsafe)
		echo "The install directory contains a quote or a newline, so no rc file was edited."
		print_path_advice
		;;
	failed)
		if [ -n "$PATH_RC" ]; then
			echo "Could not write $(rc_display), so your PATH was not changed."
		else
			echo "HOME is not set, so your PATH was not changed."
		fi
		print_path_advice
		;;
	*)
		print_path_advice
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
	case "$PATH_RESULT" in
	added | present | stale)
		case "$PATH_RC" in
		*.fish) echo "To uninstall: rm -rf $INSTALL_DIR; rm $(rc_display)" ;;
		*)
			echo "To uninstall: rm -rf $INSTALL_DIR, and delete the"
			echo "'$PATH_BLOCK_START' block from $(rc_display)"
			;;
		esac
		;;
	*) echo "To uninstall: rm -rf $INSTALL_DIR" ;;
	esac
}

main "$@"
