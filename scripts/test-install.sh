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
# The second half runs releases that carry every bundled language: install.sh
# must see each staged plugin, and scripts/refresh-mcp-install.sh must refuse
# unsafe install directories, verify the installed plugins, and stop the
# daemons of the binary it replaced. Its daemons are a stand-in compiled here
# (needs cc) and started from scratch directories, so no real g-mesh process
# is ever matched or signalled.
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
plugins) echo "typescript  $VERSION  bundled  semantic_pass=no  dir=\$(cd "\$(dirname "\$0")" && pwd -P)/plugins/typescript" ;;
esac
EOF
	chmod +x "$src/g-mesh"
	echo 'name = "typescript"' >"$src/plugins/typescript/plugin.toml"
	tar -czf "$serve/v$VERSION/$stem.tar.gz" -C "$work/src" "$stem"
	echo "$(sha256_of "$serve/v$VERSION/$stem.tar.gz")  $stem.tar.gz" \
		>"$serve/v$VERSION/$stem.tar.gz.sha256"
done

# aarch64-apple-darwin's archive is a truncated one: no bundled plugins.
stem="g-mesh-v$VERSION-aarch64-apple-darwin"
mkdir -p "$work/src/$stem"
cp "$work/src/g-mesh-v$VERSION-x86_64-apple-darwin/g-mesh" "$work/src/$stem/g-mesh"
tar -czf "$serve/v$VERSION/$stem.tar.gz" -C "$work/src" "$stem"
echo "$(sha256_of "$serve/v$VERSION/$stem.tar.gz")  $stem.tar.gz" \
	>"$serve/v$VERSION/$stem.tar.gz.sha256"

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

# An archive without plugins/typescript/plugin.toml is refused as not a
# release archive, and nothing is installed.
home="$(new_home no-bundled-plugins)"
if run_install "$home" /bin/zsh aarch64-apple-darwin --; then
	fail "no-bundled-plugins: install succeeded"
else
	before="$failures"
	for want in 'carries no plugins/typescript/plugin.toml' 'not a release archive' 'nothing was installed'; do
		grep -qF "$want" "$home.out" || fail "no-bundled-plugins: output lacks '$want'"
	done
	[[ ! -e "$home/.g-mesh/bin/g-mesh" ]] || fail "no-bundled-plugins: g-mesh was installed"
	expect_untouched no-bundled-plugins "$home"
	if [[ "$failures" -eq "$before" ]]; then
		pass "no-bundled-plugins: refused, nothing installed"
	else
		cat "$home.out" >&2
	fi
fi

# --- every bundled language: install.sh's check and refresh-mcp-install.sh ---
#
# Releases carrying every language `build-targets.sh --plugins` names, each
# under plugins/<lang>/ with a manifest whose command is the binary beside it.
# Their `g-mesh` is a stand-in that discovers plugins the way the real one
# does: ~/.g-mesh/plugins first, then plugins/ beside itself, an earlier root
# shadowing a later one, a manifest holding BROKEN listed as an error. One
# variant per failure, named rel-<variant>.

REFRESH_SH="$SCRIPT_DIR/refresh-mcp-install.sh"
# $work without the `//` a TMPDIR ending in `/` leaves in it: the refresh
# script normalizes its --install-dir, so paths it prints, and the path a
# daemon is started from, use this spelling.
wk="$(cd "$work" && pwd)"
LINUX=x86_64-unknown-linux-gnu
LANGS=()
while IFS= read -r lang; do LANGS+=("$lang"); done < <(bash "$SCRIPT_DIR/build-targets.sh" --plugins)
[[ ${#LANGS[@]} -ge 2 ]] || {
	echo "FAIL: build-targets.sh --plugins named fewer than two languages: ${LANGS[*]}" >&2
	exit 1
}

# write_fake_gmesh FILE DIR_FIELD: DIR_FIELD `dir` ends each line with
# `  dir=<canonical manifest dir>`, `nodir` prints no such field (a binary
# older than it), any other value is printed as go's dir (a binary that finds
# go somewhere other than its own plugins/).
write_fake_gmesh() {
	cat >"$1" <<EOF
#!/bin/sh
here="\$(cd "\$(dirname "\$0")" && pwd -P)"
case "\$1" in
--version) echo "g-mesh $VERSION" ;;
plugins)
	seen=" "
	for root in "\$HOME/.g-mesh/plugins" "\$here/plugins"; do
		for m in "\$root"/*/plugin.toml; do
			[ -f "\$m" ] || continue
			d="\$(cd "\$(dirname "\$m")" && pwd -P)"
			lang="\$(basename "\$d")"
			case "\$seen" in *" \$lang "*) continue ;; esac
			seen="\$seen\$lang "
			case "$2" in
			nodir) tail="" ;;
			dir) tail="  dir=\$d" ;;
			*) if [ "\$lang" = go ]; then tail="  dir=$2"; else tail="  dir=\$d"; fi ;;
			esac
			if grep -q BROKEN "\$m"; then
				echo "\$lang  error: failed to parse plugin manifest\$tail"
			else
				echo "\$lang  $VERSION  bundled  semantic_pass=no\$tail"
			fi
		done
	done
	;;
esac
EOF
	chmod +x "$1"
}

# write_plugin DIR LANG: DIR/LANG/{plugin.toml,g-mesh-plugin-LANG}.
write_plugin() {
	mkdir -p "$1/$2"
	printf '[plugin]\nlanguage = "%s"\n\n[plugin.spawn]\ncommand = "./g-mesh-plugin-%s"\n' "$2" "$2" >"$1/$2/plugin.toml"
	printf '#!/bin/sh\nexit 0\n' >"$1/$2/g-mesh-plugin-$2"
	chmod +x "$1/$2/g-mesh-plugin-$2"
}

# make_release VARIANT: $wk/rel-VARIANT/v$VERSION/<linux archive>(.sha256).
make_release() {
	local variant="$1" stem="g-mesh-v$VERSION-$LINUX" lang
	local src="$wk/relsrc-$variant/$stem" out="$wk/rel-$variant/v$VERSION"
	mkdir -p "$src/plugins" "$out"
	for lang in "${LANGS[@]}"; do write_plugin "$src/plugins" "$lang"; done
	case "$variant" in
	nodir) write_fake_gmesh "$src/g-mesh" nodir ;;
	misdirect-go) write_fake_gmesh "$src/g-mesh" /elsewhere/go ;;
	*) write_fake_gmesh "$src/g-mesh" dir ;;
	esac
	case "$variant" in
	no-go-manifest) rm "$src/plugins/go/plugin.toml" ;;
	broken-python) echo '# BROKEN' >>"$src/plugins/python/plugin.toml" ;;
	badcmd-python) sed 's|^command = .*|command = "node"|' "$src/plugins/python/plugin.toml" >"$src/p" &&
		mv "$src/p" "$src/plugins/python/plugin.toml" ;;
	noexec-rust) chmod -x "$src/plugins/rust/g-mesh-plugin-rust" ;;
	esac
	tar -czf "$out/$stem.tar.gz" -C "$wk/relsrc-$variant" "$stem"
	echo "$(sha256_of "$out/$stem.tar.gz")  $stem.tar.gz" >"$out/$stem.tar.gz.sha256"
}

for variant in good nodir misdirect-go no-go-manifest broken-python badcmd-python noexec-rust; do
	make_release "$variant"
done

# seed_state HOME: g-mesh state under HOME/.g-mesh that no install may touch.
seed_state() {
	mkdir -p "$1/.g-mesh/projects/p"
	echo "index of p" >"$1/.g-mesh/projects/p/state"
}
# state_intact NAME HOME: the seeded state is exactly what seed_state wrote.
state_intact() {
	local listing content
	listing="$(cd "$2/.g-mesh" 2>/dev/null && find . | sort | tr '\n' ' ' || true)"
	content="$(cat "$2/.g-mesh/projects/p/state" 2>/dev/null || true)"
	[[ "$listing" == ". ./projects ./projects/p ./projects/p/state " && "$content" == "index of p" ]] ||
		fail "$1: ~/.g-mesh changed (now: $listing)"
}

# install.sh, run against rel-VARIANT: run_install_rel HOME VARIANT [ARGS...]
run_install_rel() {
	local home="$1" variant="$2"
	shift 2
	run_install "$home" /bin/zsh "$LINUX" G_MESH_DOWNLOAD_BASE="file://$wk/rel-$variant" -- --no-modify-path "$@"
}

# expect_install_refused NAME HOME WANT...: install.sh failed, its output has
# every WANT, nothing was installed and no rc file written.
expect_install_refused() {
	local name="$1" home="$2" before="$failures" want
	shift 2
	for want in "$@"; do
		grep -qF "$want" "$home.out" || fail "$name: output lacks '$want'"
	done
	[[ ! -e "$home/.g-mesh/bin/g-mesh" ]] || fail "$name: g-mesh was installed"
	expect_untouched "$name" "$home"
	if [[ "$failures" -eq "$before" ]]; then pass "$name: refused"; else cat "$home.out" >&2; fi
}

# install.sh checks every staged language, not only typescript.
home="$(new_home broken-python)"
if run_install_rel "$home" broken-python; then
	fail "broken-python: install succeeded"
else
	expect_install_refused broken-python "$home" 'does not see the python plugin' 'nothing was installed'
fi

# A valid plugin of the user's own in ~/.g-mesh/plugins cannot hide a broken
# staged one: install.sh lists the staged plugins under an empty HOME.
home="$(new_home broken-python-override)"
write_plugin "$home/.g-mesh/plugins" python
if run_install_rel "$home" broken-python; then
	fail "broken-python-override: install succeeded"
else
	expect_install_refused broken-python-override "$home" 'does not see the python plugin'
fi

# The same override over an intact archive installs, with one note saying the
# override takes precedence.
home="$(new_home good-override)"
write_plugin "$home/.g-mesh/plugins" python
if run_install_rel "$home" good; then
	notes="$(grep -c 'note: your own python plugin' "$home.out" || true)"
	if [[ -x "$home/.g-mesh/bin/g-mesh" && "$notes" == 1 ]]; then
		pass "good-override: installed, one precedence note"
	else
		fail "good-override: installed=$([[ -x "$home/.g-mesh/bin/g-mesh" ]] && echo yes || echo no) notes=$notes"
		cat "$home.out" >&2
	fi
else
	fail "good-override: install failed"
	cat "$home.out" >&2
fi

# A staged binary that finds a staged language outside the stage is refused.
home="$(new_home misdirect-go)"
if run_install_rel "$home" misdirect-go; then
	fail "misdirect-go: install succeeded"
else
	expect_install_refused misdirect-go "$home" 'finds its go plugin in /elsewhere/go'
fi

# A `..` spelling of ~/.g-mesh is judged as ~/.g-mesh itself and refused.
home="$(new_home dotdot-g-mesh)"
seed_state "$home"
if run_install_rel "$home" good --install-dir "$home/nonexist/../.g-mesh"; then
	fail "dotdot-g-mesh: install.sh accepted $home/nonexist/../.g-mesh"
else
	grep -qF 'is not empty and does not look like a g-mesh install' "$home.out" ||
		fail "dotdot-g-mesh: install.sh output lacks the not-a-g-mesh-install refusal"
	pass "dotdot-g-mesh: install.sh refused"
fi
state_intact dotdot-g-mesh "$home"

# --- refresh-mcp-install.sh ---------------------------------------------------

# run_refresh HOME OUT [VAR=VALUE...] -- [ARGS...]: $refresh_under_test,
# --local --no-build from rel-good unless a DIST_DIR=... overrides it, under
# env -i. The exit status is the script's.
refresh_under_test="$REFRESH_SH"
run_refresh() {
	local home="$1" out="$2"
	shift 2
	local envs=()
	while [[ $# -gt 0 && "$1" != "--" ]]; do
		envs+=("$1")
		shift
	done
	[[ $# -gt 0 ]] && shift
	env -i \
		HOME="$home" \
		SHELL=/bin/zsh \
		PATH="$PATH" \
		TMPDIR="$wk" \
		G_MESH_VERSION="$VERSION" \
		DIST_DIR="$wk/rel-good/v$VERSION" \
		${envs[@]+"${envs[@]}"} \
		bash "$refresh_under_test" --local --no-build --target "$LINUX" "$@" \
		>"$out" 2>&1
}

# expect_refresh NAME STATUS OUT WANT...: the run exited STATUS and printed
# every WANT.
expect_refresh() {
	local name="$1" want_status="$2" status="$3" out="$4" before="$failures" want
	shift 4
	[[ "$status" == "$want_status" ]] || fail "$name: exit $status, expected $want_status"
	for want in "$@"; do
		grep -qF -- "$want" "$out" || fail "$name: output lacks '$want'"
	done
	if [[ "$failures" -eq "$before" ]]; then pass "$name"; else cat "$out" >&2; fi
}

# Refusals: no --install-dir, ~/.g-mesh in any spelling, /, HOME itself, and a
# directory holding files a g-mesh install does not own.
home="$(new_home refresh-refusals)"
seed_state "$home"
mkdir -p "$wk/foreign"
echo mine >"$wk/foreign/notes.txt"
refusal() {
	local name="$1" want="$2" status=0
	shift 2
	run_refresh "$home" "$home.$name.out" -- "$@" || status=$?
	expect_refresh "refresh refuses $name" 1 "$status" "$home.$name.out" "$want"
}
refusal no-install-dir '--install-dir DIR is required'
refusal g-mesh-bin "which holds g-mesh's own state" --install-dir "$home/.g-mesh/bin"
refusal dotdot-g-mesh "which holds g-mesh's own state" --install-dir "$home/nonexist/../.g-mesh"
refusal root 'install.sh replaces the whole directory' --install-dir /
refusal home 'install.sh replaces the whole directory' --install-dir "$home"
refusal foreign 'does not own (notes.txt)' --install-dir "$wk/foreign"
state_intact refresh-refusals "$home"
[[ "$(cat "$wk/foreign/notes.txt")" == mine ]] || fail "refresh refuses foreign: notes.txt changed"
[[ ! -e "$home/nonexist" ]] || fail "refresh refuses dotdot-g-mesh: $home/nonexist was created"
expect_untouched refresh-refusals "$home"

# A full release installs every bundled language under DIR/plugins/<lang>,
# verifies, and writes nothing into HOME (no rc file, no ~/.g-mesh).
home="$(new_home refresh-good)"
dir="$wk/mcp-good/bin"
status=0
run_refresh "$home" "$home.out" -- --install-dir "$dir" || status=$?
expect_refresh "refresh installs every language" 0 "$status" "$home.out" \
	'verified: every bundled plugin has its manifest and binary' 'no running daemons'
for lang in "${LANGS[@]}"; do
	[[ -f "$dir/plugins/$lang/plugin.toml" && -x "$dir/plugins/$lang/g-mesh-plugin-$lang" ]] ||
		fail "refresh-good: no $dir/plugins/$lang/{plugin.toml,g-mesh-plugin-$lang}"
done
! grep -qF 'skipped' "$home.out" || fail "refresh-good: the discovery-directory check was skipped"
[[ -z "$(cd "$home" && find . -mindepth 1 -print)" ]] ||
	fail "refresh-good: files were written into HOME: $(cd "$home" && find . -mindepth 1 -print)"

# A dir spelled with `.` and `..` is installed where it points.
home="$(new_home refresh-dotted)"
status=0
run_refresh "$home" "$home.out" -- --install-dir "$wk/dotted/x/./y/../bin/" || status=$?
expect_refresh "refresh installs a dotted dir where it points" 0 "$status" "$home.out" 'verified:'
[[ -x "$wk/dotted/x/bin/g-mesh" && ! -e "$wk/dotted/x/y" ]] ||
	fail "refresh-dotted: expected $wk/dotted/x/bin/g-mesh and no $wk/dotted/x/y"

# Verification failures name the language: a missing manifest, a manifest
# command outside the plugin dir, a plugin binary that is not executable.
verify_fails() {
	local variant="$1" status=0
	shift
	local home
	home="$(new_home "refresh-$variant")"
	run_refresh "$home" "$home.out" DIST_DIR="$wk/rel-$variant/v$VERSION" -- --install-dir "$wk/mcp-$variant/bin" ||
		status=$?
	expect_refresh "refresh fails on $variant" 1 "$status" "$home.out" 'verification failed' "$@"
}
verify_fails no-go-manifest "go: no $wk/mcp-no-go-manifest/bin/plugins/go/plugin.toml"
verify_fails badcmd-python "python: manifest command 'node' does not point inside"
verify_fails noexec-rust "rust: $wk/mcp-noexec-rust/bin/plugins/rust/g-mesh-plugin-rust is missing or not executable"

# A binary that predates the dir= field is verified by layout, and says so.
home="$(new_home refresh-nodir)"
status=0
run_refresh "$home" "$home.out" DIST_DIR="$wk/rel-nodir/v$VERSION" -- --install-dir "$wk/mcp-nodir/bin" || status=$?
expect_refresh "refresh verifies an older binary by layout" 0 "$status" "$home.out" 'the discovery-directory check was skipped'

# The language set is build-targets.sh --plugins: when that fails, so does the
# refresh, rather than verifying an empty or partial set.
mkdir -p "$wk/fakerepo/scripts"
cp "$REFRESH_SH" "$INSTALL_SH" "$wk/fakerepo/scripts/"
cat >"$wk/fakerepo/scripts/build-targets.sh" <<EOF
#!/bin/bash
case "\$1" in
--plugins) exit 1 ;;
--version) echo $VERSION ;;
--asset-names) echo g-mesh-v$VERSION-\$2.tar.gz ;;
esac
EOF
home="$(new_home refresh-no-languages)"
status=0
refresh_under_test="$wk/fakerepo/scripts/refresh-mcp-install.sh"
run_refresh "$home" "$home.out" -- --install-dir "$wk/mcp-nolang/bin" || status=$?
refresh_under_test="$REFRESH_SH"
expect_refresh "refresh fails when the language list cannot be read" 1 "$status" "$home.out" \
	'could not read the bundled plugin languages'

# is_installed_exe, on its own: DIR/g-mesh in either spelling, or install.sh's
# DIR.old-<digits>/g-mesh, with Linux's " (deleted)" suffix.
matcher="$(sed -n '/^is_installed_exe() {/,/^}/p' "$REFRESH_SH")"
if [[ -z "$matcher" ]]; then
	fail "is_installed_exe: not found in $REFRESH_SH"
else
	wrong="$(
		eval "$matcher"
		# shellcheck disable=SC2034 # read by the eval'd is_installed_exe
		install_dir=/s/bin
		# shellcheck disable=SC2034 # read by the eval'd is_installed_exe
		install_canon=/c/bin
		for exe_path in /s/bin/g-mesh /c/bin/g-mesh '/s/bin/g-mesh (deleted)' /s/bin.old-1234/g-mesh \
			'/s/bin.old-1234/g-mesh (deleted)' '/c/bin.old-7/g-mesh (deleted)'; do
			is_installed_exe "$exe_path" || echo "rejected $exe_path"
		done
		for exe_path in /s/bin.old-x/g-mesh /s/bin.old-/g-mesh /s/bin2/g-mesh /s/bin.old-1/sub/g-mesh \
			/s/bin/g-mesh-plugin-go /other/bin/g-mesh '/s/bin/g-mesh (deleted) '; do
			! is_installed_exe "$exe_path" || echo "accepted $exe_path"
		done
	)"
	if [[ -z "$wrong" ]]; then pass "is_installed_exe: matches only the install's binary"; else fail "is_installed_exe: $wrong"; fi
fi

# --- refresh-mcp-install.sh: daemons of the replaced binary -------------------
#
# The "old install" is a compiled stand-in run as `g-mesh daemon --project-root
# ...` (a copied system binary or a script does not show its own path as the
# process's executable on macOS). `leader` makes it lead its own process
# group; `ignore-term` makes it ignore SIGTERM.

# running PID: PID exists and is not a zombie.
running() {
	local stat
	stat="$(ps -o stat= -p "$1" 2>/dev/null | tr -d ' ')" || return 1
	[[ -n "$stat" && "$stat" != Z* ]]
}
# await_args PID WANT: up to 10s for PID's args to contain WANT (exec done).
await_args() {
	local i
	for ((i = 0; i < 100; i++)); do
		[[ "$(ps -o args= -p "$1" 2>/dev/null)" == *"$2"* ]] && return 0
		sleep 0.1
	done
	fail "pid $1 never showed '$2'"
	return 1
}
# old_install NAME: DIR=$wk/dmn-NAME/bin installed by the refresh, its g-mesh
# then swapped for the daemon stand-in; prints DIR.
old_install() {
	local d="$wk/dmn-$1/bin" h
	h="$(new_home "dmn-$1")"
	run_refresh "$h" "$h.setup.out" -- --install-dir "$d" || {
		cat "$h.setup.out" >&2
		return 1
	}
	cp "$wk/fake-daemon" "$d/g-mesh"
	echo "$d"
}

if ! command -v cc >/dev/null 2>&1; then
	echo "SKIP: daemon-stop cases (no cc to build the daemon stand-in)"
else
	cat >"$wk/fake-daemon.c" <<'EOF'
#include <signal.h>
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv) {
	for (int i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "ignore-term")) signal(SIGTERM, SIG_IGN);
		if (!strcmp(argv[i], "leader")) setpgid(0, 0);
	}
	sleep(120);
	return 0;
}
EOF
	cc -o "$wk/fake-daemon" "$wk/fake-daemon.c"
	strays=()
	kill_strays() {
		local pid
		for pid in ${strays[@]+"${strays[@]}"}; do kill -9 "$pid" 2>/dev/null || true; done
		[[ -n "${KEEP_WORK:-}" ]] || rm -rf "$work"
	}
	trap kill_strays EXIT

	# Stops a daemon of DIR/g-mesh; leaves alone DIR/g-mesh's mcp-shim and a
	# daemon of another directory's g-mesh.
	dir="$(old_install stop)"
	mkdir -p "$wk/dmn-other"
	cp "$wk/fake-daemon" "$wk/dmn-other/g-mesh"
	"$dir/g-mesh" daemon --project-root /nonexistent/stop leader &
	daemon=$!
	"$dir/g-mesh" mcp-shim &
	shim=$!
	"$wk/dmn-other/g-mesh" daemon --project-root /nonexistent/other leader &
	other=$!
	strays+=("$daemon" "$shim" "$other")
	await_args "$daemon" ' daemon --project-root ' && await_args "$other" ' daemon --project-root ' &&
		await_args "$shim" mcp-shim
	home="$(new_home dmn-stop-run)"
	status=0
	run_refresh "$home" "$home.out" -- --install-dir "$dir" || status=$?
	expect_refresh "refresh stops a daemon of the replaced binary" 0 "$status" "$home.out" 'stopped 1 daemon(s)'
	! running "$daemon" || fail "dmn-stop: the daemon $daemon still runs"
	running "$shim" || fail "dmn-stop: the mcp-shim $shim was stopped"
	running "$other" || fail "dmn-stop: the other directory's daemon $other was stopped"
	kill -9 "$daemon" "$shim" "$other" 2>/dev/null || true
	wait "$daemon" "$shim" "$other" 2>/dev/null || true

	# A daemon that ignores SIGTERM is killed after the grace period.
	dir="$(old_install kill)"
	"$dir/g-mesh" daemon --project-root /nonexistent/kill ignore-term leader &
	daemon=$!
	strays+=("$daemon")
	await_args "$daemon" ' daemon --project-root '
	home="$(new_home dmn-kill-run)"
	status=0
	run_refresh "$home" "$home.out" G_MESH_REFRESH_TERM_GRACE=1 G_MESH_REFRESH_KILL_GRACE=5 -- --install-dir "$dir" ||
		status=$?
	expect_refresh "refresh kills a daemon that ignores SIGTERM" 0 "$status" "$home.out" \
		'ignored SIGTERM' 'stopped 1 daemon(s)'
	! running "$daemon" || fail "dmn-kill: the daemon $daemon still runs"
	wait "$daemon" 2>/dev/null || true

	# A killed daemon whose parent never reaps it stays a zombie, which is not
	# a survivor. It is not a group leader, so only its pid is signalled.
	dir="$(old_install zombie)"
	sh -c '"$1" daemon --project-root /nonexistent/zombie ignore-term & exec sleep 120' _ "$dir/g-mesh" &
	parent=$!
	strays+=("$parent")
	daemon=""
	for ((i = 0; i < 100; i++)); do
		daemon="$(ps -axo pid=,ppid=,args= | awk -v p="$parent" '$2 == p && / daemon --project-root / { print $1 }')"
		[[ -n "$daemon" ]] && break
		sleep 0.1
	done
	if [[ -z "$daemon" ]]; then
		fail "dmn-zombie: the daemon under $parent never started"
	else
		strays+=("$daemon")
		home="$(new_home dmn-zombie-run)"
		status=0
		run_refresh "$home" "$home.out" G_MESH_REFRESH_TERM_GRACE=1 G_MESH_REFRESH_KILL_GRACE=5 -- --install-dir "$dir" ||
			status=$?
		expect_refresh "refresh counts a killed, unreaped daemon as stopped" 0 "$status" "$home.out" 'stopped 1 daemon(s)'
		! running "$daemon" || fail "dmn-zombie: the daemon $daemon still runs"
	fi
	kill -9 "$parent" 2>/dev/null || true
	wait "$parent" 2>/dev/null || true
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
