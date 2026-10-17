# shellcheck shell=bash
#
# GM-628: the plugins' test dependencies (vtsls, pyright) are found on PATH
# only (ADR 0032 D7); CI puts each plugin's node_modules/.bin there in its
# "Install vtsls" step, so the local runs do too. Sourced by
# scripts/test-local.sh (GM-598) and scripts/cut-release.sh, whose test run
# used to rely on a global vtsls and failed core-it without one (4.10.0).
#
# The sourcing script defines `die` (its own prefix and exit code), so a
# missing directory reads as that script's error.

# plugin_bins_on_path <repo-root> <required>: prepends
# <repo-root>/plugins/{typescript,python}/node_modules/.bin to PATH and
# exports it. Paths come from <repo-root>, never the working directory. A
# missing directory is fatal (`die`, naming scripts/test-deps.sh) when
# <required> is 1, and skipped when it is 0 (a dry run, which runs no test).
plugin_bins_on_path() {
	local root="$1" required="$2" plugin bin_dir plugin_bin_dirs=""
	for plugin in typescript python; do
		bin_dir="$root/plugins/$plugin/node_modules/.bin"
		if [ -d "$bin_dir" ]; then
			plugin_bin_dirs="${plugin_bin_dirs:+$plugin_bin_dirs:}$bin_dir"
		elif [ "$required" -eq 1 ]; then
			die "plugins/$plugin/node_modules/.bin is missing; run scripts/test-deps.sh"
		fi
	done
	[ -z "$plugin_bin_dirs" ] || export PATH="$plugin_bin_dirs:$PATH"
}
