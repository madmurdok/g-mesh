#!/usr/bin/env bash
#
# Installs or refreshes g-mesh in a fixed directory an MCP client runs it
# from (e.g. ~/.local/share/g-mesh/bin), and proves the result discovers
# every bundled language plugin from that directory.
#
# The install itself is scripts/install.sh (checksum, pre-install run of the
# staged binary, atomic directory swap); this script only picks the source,
# verifies the effect afterwards, and stops the daemons still running the
# replaced binary.
#
#   scripts/refresh-mcp-install.sh --install-dir DIR                 # latest published release
#   scripts/refresh-mcp-install.sh --install-dir DIR --version X.Y.Z # that published release
#   scripts/refresh-mcp-install.sh --install-dir DIR --local         # build this checkout, install it
#
# Options:
#   --install-dir DIR    required; replaced as a whole. Refused when it is, or
#                        is under, ~/.g-mesh (the default install's state
#                        lives there), or when it holds files a g-mesh
#                        install does not own.
#   --version X.Y.Z      a published release (default: the latest one)
#   --local              build this checkout with scripts/build-targets.sh
#                        and install that archive over file://
#   --no-build           with --local: reuse the archive already in DIST_DIR
#   --target TRIPLE      default: this host's triple
#   --no-stop-daemons    leave the running daemons of DIR/g-mesh alone
#
# Never edits shell rc files (install.sh always gets --no-modify-path) and
# never writes under ~/.g-mesh.
#
# Exit status: 0 installed and verified; 1 refused, install failed, or
# verification failed; 3 installed and verified, but daemons of the replaced
# binary are still running.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_SH="$SCRIPT_DIR/install.sh"
BUILD_TARGETS="$SCRIPT_DIR/build-targets.sh"

# Daemon stop: SIGTERM to the daemon's process group, SIGKILL to what is left
# after TERM_GRACE seconds, then KILL_GRACE seconds before reporting survivors.
TERM_GRACE="${G_MESH_REFRESH_TERM_GRACE:-10}"
KILL_GRACE="${G_MESH_REFRESH_KILL_GRACE:-3}"

die() {
	echo "refresh-mcp-install: error: $*" >&2
	exit 1
}

log() {
	echo "==> $*"
}

usage() {
	sed -n '2,/^$/s/^# \{0,1\}//p' "${BASH_SOURCE[0]}"
}

# canonical PATH: PATH with symlinks resolved, also for a PATH that does not
# exist yet (its nearest existing ancestor is resolved, the rest appended).
canonical() {
	local path="$1" rest=""
	while [[ ! -d "$path" ]]; do
		rest="/$(basename "$path")$rest"
		path="$(dirname "$path")"
	done
	printf '%s%s\n' "$(cd "$path" && pwd -P)" "$rest"
}

# under PATH BASE: PATH is BASE or inside it.
under() {
	[[ "$1" == "$2" || "$1" == "$2"/* ]]
}

install_dir=""
version=""
local_build=0
no_build=0
target=""
stop_daemons=1

while [[ $# -gt 0 ]]; do
	case "$1" in
	--install-dir)
		[[ $# -ge 2 ]] || die "--install-dir needs a directory"
		install_dir="$2"
		shift 2
		;;
	--version)
		[[ $# -ge 2 ]] || die "--version needs a version"
		version="${2#v}"
		shift 2
		;;
	--local)
		local_build=1
		shift
		;;
	--no-build)
		no_build=1
		shift
		;;
	--target)
		[[ $# -ge 2 ]] || die "--target needs a triple"
		target="$2"
		shift 2
		;;
	--no-stop-daemons)
		stop_daemons=0
		shift
		;;
	-h | --help)
		usage
		exit 0
		;;
	*) die "unknown argument: $1 (see --help)" ;;
	esac
done

[[ -n "$install_dir" ]] || die "--install-dir DIR is required (see --help)"
[[ -n "${HOME:-}" ]] || die "HOME is not set"
if [[ "$local_build" == 1 && -n "$version" ]]; then
	die "--local installs this checkout's version; drop --version"
fi
if [[ "$no_build" == 1 && "$local_build" == 0 ]]; then
	die "--no-build only applies to --local"
fi

# --- the install directory -------------------------------------------------

[[ "$install_dir" == /* ]] || install_dir="$PWD/$install_dir"
while [[ "$install_dir" == */ && "$install_dir" != / ]]; do
	install_dir="${install_dir%/}"
done
install_canon="$(canonical "$install_dir")"
home_canon="$(canonical "$HOME")"

for refused in "$HOME/.g-mesh" "$home_canon/.g-mesh"; do
	if under "$install_dir" "$refused" || under "$install_canon" "$refused"; then
		die "refusing $install_dir: it is under $refused, which holds g-mesh's own state; pick a directory outside it (e.g. ~/.local/share/g-mesh/bin)"
	fi
done
for refused in / "$HOME" "$home_canon"; do
	if [[ "$install_dir" == "$refused" || "$install_canon" == "$refused" ]]; then
		die "refusing $install_dir: install.sh replaces the whole directory"
	fi
done

# install.sh replaces DIR as a whole and deletes the previous one, so DIR may
# hold only what a g-mesh install puts there (plus bare plugin binaries a flat
# copy left behind, which the new layout replaces).
if [[ -d "$install_dir" ]]; then
	foreign=()
	for entry in "$install_dir"/* "$install_dir"/.[!.]*; do
		[[ -e "$entry" || -L "$entry" ]] || continue
		case "$(basename "$entry")" in
		g-mesh | plugins | g-mesh-plugin-* | LICENSE* | README.md) ;;
		*) foreign+=("$(basename "$entry")") ;;
		esac
	done
	if [[ ${#foreign[@]} -gt 0 ]]; then
		die "refusing $install_dir: it holds files a g-mesh install does not own (${foreign[*]}), and installing replaces the whole directory"
	fi
elif [[ -e "$install_dir" ]]; then
	die "refusing $install_dir: it exists and is not a directory"
fi

if [[ -z "$target" ]]; then
	command -v rustc >/dev/null 2>&1 && target="$(rustc -vV | awk '/^host: / { print $2 }')"
fi

# --- install ---------------------------------------------------------------

install_args=(--install-dir "$install_dir" --no-modify-path)
[[ -n "$target" ]] && install_args+=(--target "$target")

if [[ "$local_build" == 1 ]]; then
	[[ -n "$target" ]] || die "--local needs --target (no rustc on PATH to find this host's triple)"
	version="$(bash "$BUILD_TARGETS" --version)"
	archive="$(bash "$BUILD_TARGETS" --asset-names "$target" | awk 'NR == 1')"
	dist_dir="${DIST_DIR:-$(cd "$SCRIPT_DIR/.." && pwd)/dist}"
	if [[ "$no_build" == 0 ]]; then
		log "building $version for $target (scripts/build-targets.sh)"
		bash "$BUILD_TARGETS" "$target"
	fi
	[[ -f "$dist_dir/$archive" && -f "$dist_dir/$archive.sha256" ]] ||
		die "no $archive (+ .sha256) in $dist_dir"
	serve="$(mktemp -d "${TMPDIR:-/tmp}/g-mesh-refresh-serve.XXXXXX")"
	trap 'rm -rf "$serve"' EXIT
	mkdir -p "$serve/v$version"
	cp "$dist_dir/$archive" "$dist_dir/$archive.sha256" "$serve/v$version/"
	log "installing local build $version into $install_dir"
	G_MESH_DOWNLOAD_BASE="file://$serve" sh "$INSTALL_SH" "${install_args[@]}" --version "$version"
else
	[[ -n "$version" ]] && install_args+=(--version "$version")
	log "installing published release ${version:-(latest)} into $install_dir"
	sh "$INSTALL_SH" "${install_args[@]}"
fi

# --- verify the effect -------------------------------------------------------

exe="$install_dir/g-mesh"
[[ -x "$exe" ]] || die "verification failed: no executable $exe after the install"
log "installed: $("$exe" --version)"

plugins_list="$("$exe" plugins list)" || die "verification failed: $exe plugins list exited non-zero"
plugins_canon="$(canonical "$install_dir/plugins")"
user_plugins_canon="$(canonical "$HOME/.g-mesh/plugins")"
dir_checked=0
failures=()

while IFS= read -r lang; do
	[[ -n "$lang" ]] || continue
	plugin_dir="$install_dir/plugins/$lang"
	manifest="$plugin_dir/plugin.toml"
	if [[ ! -f "$manifest" ]]; then
		failures+=("$lang: no $manifest")
		continue
	fi
	# The first `command` in a manifest is the plugin's own; later ones belong
	# to its semantic tier's language server.
	command="$(sed -n 's/^command[[:space:]]*=[[:space:]]*"\(.*\)"[[:space:]]*$/\1/p' "$manifest" | awk 'NR == 1')"
	case "$command" in
	./*) command_path="$plugin_dir/${command#./}" ;;
	/*) command_path="$command" ;;
	*) command_path="" ;;
	esac
	if [[ -z "$command_path" ]] || ! under "$(canonical "$command_path")" "$(canonical "$plugin_dir")"; then
		failures+=("$lang: manifest command '$command' does not point inside $plugin_dir")
	elif [[ ! -f "$command_path" || ! -x "$command_path" ]]; then
		failures+=("$lang: $command_path is missing or not executable")
	fi

	line="$(printf '%s\n' "$plugins_list" | grep -E "^$lang +[0-9]" | awk 'NR == 1' || true)"
	if [[ -z "$line" ]]; then
		failures+=("$lang: $exe plugins list does not list it as loaded")
		continue
	fi
	if [[ "$line" == *"  dir="* ]]; then
		dir_checked=1
		listed_dir="$(canonical "${line##*  dir=}")"
		if [[ "$listed_dir" == "$user_plugins_canon/$lang" ]]; then
			log "note: $lang is served by your own override in $listed_dir, not by the plugin installed here"
		elif [[ "$listed_dir" != "$plugins_canon/$lang" ]]; then
			failures+=("$lang: $exe finds it in $listed_dir, not in $plugin_dir")
		fi
	fi
done < <(bash "$BUILD_TARGETS" --plugins)

if [[ ${#failures[@]} -gt 0 ]]; then
	printf 'refresh-mcp-install: verification failed:\n' >&2
	printf '  %s\n' "${failures[@]}" >&2
	exit 1
fi
if [[ "$dir_checked" == 1 ]]; then
	log "verified: every bundled plugin has its manifest and binary under $install_dir/plugins and is discovered from there"
else
	log "verified: every bundled plugin has its manifest and binary under $install_dir/plugins and is listed; the discovery-directory check was skipped ($exe predates 'plugins list' printing dir=)"
fi

# --- daemons of the replaced binary ----------------------------------------

# Prints "PID PGID" for every `g-mesh daemon` process whose executable is
# exactly DIR/g-mesh (as given or canonical). Never matches by pattern.
daemons_of_install() {
	local pid pgid path args
	if [[ -e /proc/self/exe ]]; then
		for proc in /proc/[0-9]*; do
			path="$(readlink "$proc/exe" 2>/dev/null)" || continue
			path="${path% (deleted)}"
			[[ "$path" == "$exe" || "$path" == "$install_canon/g-mesh" ]] || continue
			pid="${proc#/proc/}"
			args="$(tr '\0' ' ' <"$proc/cmdline" 2>/dev/null)" || continue
			[[ "$args" == *" daemon --project-root "* ]] || continue
			pgid="$(ps -o pgid= -p "$pid" 2>/dev/null | tr -d ' ')" || continue
			echo "$pid $pgid"
		done
	else
		# macOS: `comm` is the full path the process was executed from.
		while read -r pid pgid path; do
			[[ "$path" == "$exe" || "$path" == "$install_canon/g-mesh" ]] || continue
			args="$(ps -o args= -p "$pid" 2>/dev/null)" || continue
			[[ "$args" == *" daemon --project-root "* ]] || continue
			echo "$pid $pgid"
		done < <(ps -axo pid=,pgid=,comm=)
	fi
}

# alive PID PGID: the daemon, or anything left in the group it leads, runs.
alive() {
	if [[ "$2" == "$1" ]]; then
		kill -0 -- "-$2" 2>/dev/null
	else
		kill -0 "$1" 2>/dev/null
	fi
}

signal() {
	if [[ "$3" == "$2" ]]; then
		kill "-$1" -- "-$3" 2>/dev/null || true
	else
		kill "-$1" "$2" 2>/dev/null || true
	fi
}

# wait_gone SECONDS: polls the daemons in `pending` until none is alive or
# SECONDS pass; leaves the survivors in `pending`.
wait_gone() {
	local deadline=$((SECONDS + $1)) left entry
	while :; do
		left=()
		for entry in "${pending[@]}"; do
			# shellcheck disable=SC2086
			if alive $entry; then left+=("$entry"); fi
		done
		pending=()
		[[ ${#left[@]} -eq 0 ]] && return 0
		pending=("${left[@]}")
		[[ $SECONDS -ge $deadline ]] && return 0
		sleep 0.2
	done
}

found=()
while IFS= read -r entry; do
	found+=("$entry")
done < <(daemons_of_install)
if [[ ${#found[@]} -eq 0 ]]; then
	log "no running daemons of $exe"
	exit 0
fi
if [[ "$stop_daemons" == 0 ]]; then
	log "${#found[@]} daemon(s) of the replaced $exe still run (--no-stop-daemons); stop them for the new build to answer:"
	for entry in "${found[@]}"; do
		echo "  pid ${entry%% *}: $(ps -o args= -p "${entry%% *}" 2>/dev/null || echo '?')"
	done
	exit 0
fi

log "stopping ${#found[@]} daemon(s) of the replaced $exe"
for entry in "${found[@]}"; do
	# shellcheck disable=SC2086
	signal TERM $entry
done
pending=("${found[@]}")
wait_gone "$TERM_GRACE"
if [[ ${#pending[@]} -gt 0 ]]; then
	log "${#pending[@]} daemon(s) ignored SIGTERM for ${TERM_GRACE}s; sending SIGKILL"
	for entry in "${pending[@]}"; do
		# shellcheck disable=SC2086
		signal KILL $entry
	done
	wait_gone "$KILL_GRACE"
fi
if [[ ${#pending[@]} -gt 0 ]]; then
	echo "refresh-mcp-install: ${#pending[@]} daemon(s) of the replaced binary are still running:" >&2
	for entry in "${pending[@]}"; do
		echo "  pid ${entry%% *} (group ${entry##* }): $(ps -o args= -p "${entry%% *}" 2>/dev/null || echo 'its group still has members')" >&2
	done
	exit 3
fi
log "stopped ${#found[@]} daemon(s); MCP clients start the new build on their next call (or reconnect with /mcp)"
