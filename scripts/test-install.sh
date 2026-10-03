#!/usr/bin/env bash
#
# Tests install.sh's PATH editing without a release and without
# touching the real shell rc files of whoever runs it.
#
# Each case runs install.sh under `env -i` with HOME pointed at a fresh temp
# directory, so ~/.zshrc and friends resolve inside it, and with nothing else
# from the caller's environment (no ZDOTDIR, no XDG_CONFIG_HOME, no
# G_MESH_*) leaking in. The "release" is a fake archive built here: a
# `g-mesh` shell script that answers `--version` and `plugins list` the way
# install.sh's pre-install check expects, served over file:// through the
# documented G_MESH_DOWNLOAD_BASE override. --target picks the archive, which
# is also what decides install.sh's bash-on-macOS rc file, so both bash cases
# run on any host.
#
# As a last guard, the real rc files' checksums are taken before the run and
# compared after it: if any test ever leaks into the real HOME, this fails.
#
# Usage: bash scripts/test-install.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_SH="$SCRIPT_DIR/install.sh"
VERSION=9.9.9
START='# >>> g-mesh installer >>>'
END='# <<< g-mesh installer <<<'

work="$(mktemp -d "${TMPDIR:-/tmp}/g-mesh-test-install.XXXXXX")"
trap '[[ -n "${KEEP_WORK:-}" ]] || rm -rf "$work"' EXIT

real_home="${HOME:-}"
real_rc_sums() {
	local f
	for f in .zshrc .bashrc .bash_profile .bash_login .profile .config/fish/conf.d/g-mesh.fish; do
		if [[ -n "$real_home" && -e "$real_home/$f" ]]; then
			printf '%s %s\n' "$f" "$(cksum <"$real_home/$f")"
		fi
	done
}
real_before="$(real_rc_sums)"

failures=0
fail() {
	echo "FAIL: $*" >&2
	failures=$((failures + 1))
}
pass() {
	echo "PASS: $*"
}

# --- the fake release ------------------------------------------------------

sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | awk '{ print $1 }'
	else
		shasum -a 256 "$1" | awk '{ print $1 }'
	fi
}

serve="$work/serve"
for target in x86_64-unknown-linux-gnu x86_64-apple-darwin; do
	stem="g-mesh-v$VERSION-$target"
	src="$work/src/$stem"
	mkdir -p "$src/plugins/typescript" "$serve/v$VERSION"
	cat >"$src/g-mesh" <<EOF
#!/bin/sh
case "\$1" in
--version) echo "g-mesh $VERSION" ;;
plugins) echo "typescript (bundled)" ;;
esac
EOF
	chmod +x "$src/g-mesh"
	echo 'name = "typescript"' >"$src/plugins/typescript/plugin.toml"
	tar -czf "$serve/v$VERSION/$stem.tar.gz" -C "$work/src" "$stem"
	echo "$(sha256_of "$serve/v$VERSION/$stem.tar.gz")  $stem.tar.gz" \
		>"$serve/v$VERSION/$stem.tar.gz.sha256"
done

# --- helpers ---------------------------------------------------------------

# new_home NAME: a fresh, empty HOME for one case.
new_home() {
	local h="$work/home-$1"
	mkdir -p "$h"
	echo "$h"
}

# run_install HOME SHELL TARGET [VAR=VALUE...] -- [install.sh args...]
# Output goes to $HOME.out; the exit status is install.sh's.
run_install() {
	local home="$1" shell="$2" target="$3"
	shift 3
	local envs=()
	while [[ $# -gt 0 && "$1" != "--" ]]; do
		envs+=("$1")
		shift
	done
	[[ $# -gt 0 ]] && shift
	env -i \
		HOME="$home" \
		SHELL="$shell" \
		PATH="${TEST_PATH:-$PATH}" \
		TMPDIR="$work" \
		G_MESH_DOWNLOAD_BASE="file://$serve" \
		${envs[@]+"${envs[@]}"} \
		sh "$INSTALL_SH" --version "$VERSION" --target "$target" "$@" \
		>"$home.out" 2>&1
}

# Every file under HOME except the install itself, one per line.
home_files() {
	(cd "$1" && find . -path ./.g-mesh -prune -o -type f -print | sort)
}

# expect_one_block NAME HOME FILE KIND: FILE has exactly one marked block,
# with the PATH line for the install dir in it, and is the only file the
# installer created outside ~/.g-mesh.
expect_one_block() {
	local name="$1" home="$2" file="$3" kind="$4"
	local dir="$home/.g-mesh/bin" line
	if [[ "$kind" == fish ]]; then
		line="fish_add_path -g '$dir'"
	else
		line="export PATH='$dir':\"\$PATH\""
	fi
	if [[ ! -f "$file" ]]; then
		fail "$name: $file was not created"
		cat "$home.out" >&2
		return
	fi
	local starts ends lines
	starts="$(grep -cxF "$START" "$file" || true)"
	ends="$(grep -cxF "$END" "$file" || true)"
	lines="$(grep -cxF "$line" "$file" || true)"
	[[ "$starts" == 1 && "$ends" == 1 && "$lines" == 1 ]] ||
		fail "$name: expected one marked block with the PATH line in $file, got start=$starts end=$ends line=$lines"
	grep -qF "${file#"$home"/}" "$home.out" ||
		fail "$name: the summary does not name ${file#"$home"/}"
	grep -qi 'restart your shell' "$home.out" ||
		fail "$name: the summary does not say to restart the shell"
}

# expect_untouched NAME HOME: no file outside ~/.g-mesh, and the old advice.
expect_untouched() {
	local name="$1" home="$2" files
	files="$(home_files "$home")"
	[[ -z "$files" ]] || fail "$name: files were written outside ~/.g-mesh: $files"
}

# --- cases -----------------------------------------------------------------

# fresh_and_again NAME SHELL TARGET RC-RELATIVE KIND [SEED-FILE SEED]
fresh_and_again() {
	local name="$1" shell="$2" target="$3" rc="$4" kind="$5"
	local home
	home="$(new_home "$name")"
	if [[ $# -ge 7 ]]; then
		printf '%s' "$7" >"$home/$6"
	fi
	if ! run_install "$home" "$shell" "$target" --; then
		fail "$name: install failed"
		cat "$home.out" >&2
		return
	fi
	expect_one_block "$name" "$home" "$home/$rc" "$kind"
	[[ -f "$home/$rc" ]] || return 0
	local others
	others="$(home_files "$home" | grep -vxF "./$rc" || true)"
	[[ -z "$others" ]] || fail "$name: unexpected files besides $rc: $others"

	if [[ "$kind" == sh ]]; then
		# Sourcing the block must actually put the directory on PATH.
		local got
		# shellcheck disable=SC2016 # expanded by the inner sh, on purpose
		got="$(env -i HOME="$home" PATH=/usr/bin:/bin sh -c '. "$1" >/dev/null 2>&1; echo "$PATH"' _ "$home/$rc")"
		[[ ":$got:" == *":$home/.g-mesh/bin:"* ]] ||
			fail "$name: sourcing $rc did not put the install dir on PATH (PATH=$got)"
	fi

	cp "$home/$rc" "$home.first"
	if ! run_install "$home" "$shell" "$target" --; then
		fail "$name: second install failed"
		cat "$home.out" >&2
		return
	fi
	cmp -s "$home/$rc" "$home.first" || fail "$name: a second install changed $rc"
	grep -q 'left unchanged' "$home.out" || fail "$name: second install did not report the block as already present"
	pass "$name: one block in $rc, unchanged by a second install"
}

fresh_and_again zsh /bin/zsh x86_64-unknown-linux-gnu .zshrc sh
fresh_and_again bash-linux /bin/bash x86_64-unknown-linux-gnu .bashrc sh
fresh_and_again bash-macos /bin/bash x86_64-apple-darwin .bash_profile sh
fresh_and_again fish /usr/bin/fish x86_64-unknown-linux-gnu .config/fish/conf.d/g-mesh.fish fish
fresh_and_again other-shell /bin/tcsh x86_64-unknown-linux-gnu .profile sh

# An existing rc file whose last line has no newline: the user's line stays
# intact and the block starts on a line of its own.
fresh_and_again zsh-no-trailing-newline /bin/zsh x86_64-unknown-linux-gnu .zshrc sh .zshrc 'alias ll=ls'
grep -qxF 'alias ll=ls' "$work/home-zsh-no-trailing-newline/.zshrc" ||
	fail "zsh-no-trailing-newline: the user's last line was not preserved as its own line"

# macOS bash with an existing ~/.profile and no ~/.bash_profile: writing a new
# ~/.bash_profile would stop bash from reading ~/.profile, so the block goes
# into ~/.profile instead.
fresh_and_again bash-macos-profile /bin/bash x86_64-apple-darwin .profile sh .profile $'# mine\n'
[[ ! -e "$work/home-bash-macos-profile/.bash_profile" ]] ||
	fail "bash-macos-profile: ~/.bash_profile was created beside an existing ~/.profile"

# untouched NAME SHELL ADVICE-PATTERN [VAR=VALUE...] -- [ARGS...]
untouched() {
	local name="$1" shell="$2" pattern="$3"
	shift 3
	local home
	home="$(new_home "$name")"
	if ! run_install "$home" "$shell" x86_64-unknown-linux-gnu "$@"; then
		fail "$name: install failed"
		cat "$home.out" >&2
		return
	fi
	[[ -x "$home/.g-mesh/bin/g-mesh" ]] || fail "$name: g-mesh was not installed"
	expect_untouched "$name" "$home"
	grep -qF "$pattern" "$home.out" || fail "$name: output lacks '$pattern'"
	pass "$name: no rc file touched"
}

untouched no-modify-path-flag /bin/zsh 'export PATH="' -- --no-modify-path
untouched no-modify-path-env /bin/zsh 'export PATH="' G_MESH_NO_MODIFY_PATH=1 --
untouched no-modify-path-env-fish /usr/bin/fish 'fish_add_path' G_MESH_NO_MODIFY_PATH=1 --
TEST_PATH="$work/home-already-on-path/.g-mesh/bin:$PATH" \
	untouched already-on-path /bin/zsh 'already on your PATH' --

# G_MESH_NO_MODIFY_PATH=0 means "do modify", the same as unset.
home="$(new_home env-zero)"
if run_install "$home" /bin/zsh x86_64-unknown-linux-gnu G_MESH_NO_MODIFY_PATH=0 --; then
	expect_one_block env-zero "$home" "$home/.zshrc" sh
	pass "env-zero: G_MESH_NO_MODIFY_PATH=0 still edits ~/.zshrc"
else
	fail "env-zero: install failed"
fi

# --- the real HOME ---------------------------------------------------------

real_after="$(real_rc_sums)"
if [[ "$real_before" != "$real_after" ]]; then
	fail "the REAL rc files under $real_home changed during this run:
before:
$real_before
after:
$real_after"
fi

if [[ "$failures" -ne 0 ]]; then
	echo "$failures failure(s)" >&2
	exit 1
fi
echo "all install.sh PATH tests passed"
