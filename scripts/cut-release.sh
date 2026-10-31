#!/usr/bin/env bash
#
# One command to validate, test and tag a release, instead of the four manual
# steps described in .github/workflows/release.yml's header (check the crate
# version, run the workflow once by hand, tag; release.yml publishes the
# release). The version
# is typed here exactly once.
#
#   scripts/cut-release.sh <version>              # verify, wait for green CI, tag locally
#   scripts/cut-release.sh <version> --push       # ...and push the tag
#   scripts/cut-release.sh <version> --ci-timeout 120 # wait up to 120 min for a running ci.yml run (default 90, 0 = no wait)
#
# This script does NOT bump the version - that already happened as the
# release branch's first commit. What it does instead is VERIFY that
# `core/Cargo.toml` already says <version>, using the exact check
# .github/workflows/release.yml's "Check the tag matches the crate version"
# step runs (`build-targets.sh --version`), and refuse otherwise. That check
# is the whole point: it is the guard against the tag and the crate disagreeing
# - the failure #197 recorded, where the crate sat at 2.0.0 through five
# releases because nothing caught it.
#
# GM-288: since the repository became a cargo workspace (GM-284), `core/
# Cargo.toml` is not the only manifest with a hand-pinned `version` -
# `wire/Cargo.toml`, `plugins/sdk/Cargo.toml`, `plugins/rust/Cargo.toml` and,
# since GM-298, `plugins/python/Cargo.toml` (and since GM-514 `walk/Cargo.toml`)
# each carry their own. Nothing
# forces them to agree with core's, and a crate
# whose version silently drifts is the same class of failure #197 already
# named, just in a manifest this script did not use to look at. Rather than
# switching every member to `version.workspace = true` (root-Cargo.toml
# inheritance, which would remove the possibility of drift entirely but also
# ripple through every comment and script that currently reads `core/
# Cargo.toml`'s own `[package] version` as the release's version of record -
# `build-targets.sh`, `prepare-release-assets.sh`, `release.yml`, this file,
# and README.md's own release checklist), this script instead grows the same
# check it already runs to cover every workspace member: `core/Cargo.toml`
# stays the one manifest whose version *names* the release, and wire's,
# walk's and plugins/sdk's versions are checked to equal it (the three
# language plugins carry their own, see below), in `check_workspace_versions`
# below, before a single test runs. A member added to the workspace later
# that forgets this gets caught here, not after a release ships with it
# silently stale - which is the whole reason this got written down instead of
# only fixed: this decision is what the next language plugin's manifest
# should follow too.
#
# `plugin.toml`'s own `plugin_version` is a different field from every
# manifest `[package] version` above, checked by a different rule. `g-mesh
# plugins list` prints it and core names it in a protocol-mismatch message,
# so it is the number a person reads first about a plugin. It names the
# plugin build, never the release: the release archive's asset names carry
# core's version, and a plugin's `plugin_version` changes only when the
# plugin does.
#
#   - plugins/rust, plugins/python and plugins/typescript are cargo workspace
#     members that carry their own version: `plugin_version` equals the
#     crate's `[package] version` (each crate's
#     `the_manifest_version_matches_the_crates` test pins this too), and the
#     two are bumped together only when the plugin changed since the previous
#     release tag. `check_crate_backed_plugin_versions` below enforces both:
#     the agreement, and the change rule. A plugin changed when its shipped
#     inputs differ from the tag: its own and plugins/sdk's, wire's and walk's
#     `src/`; its own `Cargo.toml` and `plugin.toml` and sdk's, wire's and
#     walk's `Cargo.toml`, each with its own version line ignored (so the
#     release bump of sdk/wire/walk is not a change); and the registry/git
#     packages its `Cargo.lock` dependency closure reaches. Tests, fixtures,
#     READMEs and every other path do not count. A changed plugin whose
#     version did not move, or a version lower than at the tag, stops the cut;
#     a bump without a change is accepted and logged.
#   - plugins/go is NOT a workspace member - a separate Go module, bumped by
#     hand for the plugin's own capability changes (plugins/go/wire.go's
#     `pluginVersion` const). What `check_self_versioned_plugin_versions`
#     below checks is internal agreement: plugin.toml's copy against the
#     `pluginVersion` const. `TestPluginVersionMatchesTheManifest` in
#     plugins/go/manifest_test.go tests this too; this script re-checks it by
#     reading the files, so drift is still caught without a build and
#     without a Go toolchain on PATH.
#
# docs/architecture/plugin-modularity.md ("plugin_version: two rules, not
# one") is where a plugin author reads this before adding a plugin; this
# comment is the enforcement side of that same rule.
#
# It tags `main`, and only after the release branch has already been merged
# into it - the tag is meant to point at the code someone gets by cloning, not
# at a branch tip nobody else can see. It does not create the release branch,
# does not merge anything, does not publish the Release GitHub builds
# (release.yml publishes it when all four builds pass), and does not touch
# the task tracker.
#
# Pushing the tag starts a public four-platform build and a Release, so
# it takes the explicit --push above; without it, the tag is created locally
# and the exact command to push it is printed instead.
#
# GM-675: this script runs no test suite. The release gate is GitHub CI: a
# full ci.yml run (every section, doctests and `cargo test -p g-mesh` on the
# Linux row) that passed on the release branch tip `main` merged, with the
# merge's tree equal to that tip's tree, so the tested code is the tagged code.
# A run still in progress is waited for (poll every 60 s, up to --ci-timeout
# minutes), so the wait is the last step before `git tag` and every cheap check
# fails first. The batch-end step dispatches that run
# (`gh workflow run ci.yml --ref release-<v>`); this script only reads it.
# The gh calls go through $GH (default `gh`, as scripts/ci-green-for-commit.sh) so tests can inject a
# fake; CUT_RELEASE_CI_POLL_SECS overrides the 60 s poll interval.
#
# GM-302: the workspace-wide test run is GM-288's deliberate choice, but a
# workspace is not the configuration that ships. `build-targets.sh` builds
# core package-scoped, from `core/` (`cd core && cargo build`, no `-p`) -
# `plugins/rust` and `plugins/python` (and whatever THEY pull in) are not in
# that graph, whereas `--workspace` builds every one of them alongside core in
# the same cargo invocation. Cargo unifies features across everything built
# together, so this is a real question, not a hypothetical one: does anything
# plugins/rust or plugins/python depend on change what CORE itself links?
#
# The concrete instance that prompted this: `plugins/rust` and
# `plugins/python` both depend on `tree-sitter`, which depends on
# `serde_json` with its `preserve_order` feature (swaps `serde_json::Map`'s
# backing store from `BTreeMap`, sorted, to `IndexMap`, insertion-ordered).
#
#   cargo tree -p g-mesh -e features | grep -c preserve_order   -> 0
#   cargo tree              -e features | grep -c preserve_order   -> 1
#
# Read at face value that says core's own JSON key order differs between the
# two builds - a real concern, since MCP tool results and schemas are JSON.
# MEASURED rather than stopped at that reading (this repo's own house rule:
# an argument from a `cargo tree` line is worth less than checking what
# actually gets linked): `tree-sitter`'s need for `preserve_order` is a
# BUILD-dependency of tree-sitter's own code generation, and cargo's
# resolver v2 keeps build-dependencies and proc-macros (`rmcp-macros` needs
# the same feature, as a proc-macro) in a separate "host artifact" feature
# resolution from the normal/target graph a linked binary or library actually
# draws from - which is exactly why `cargo tree -e normal,features,
# no-proc-macro` (excluding precisely those two host-only edge kinds) reads 0
# on BOTH the scoped and the workspace view. Confirmed three more ways, not
# just argued: (1) `cargo build --workspace -v`'s rustc invocation for the
# `g-mesh` bin, AND for core's own lib/test targets under `cargo test
# --workspace`, both pass `--extern serde_json=` the identical compiled unit
# (features `["alloc","default","std"]`, no `preserve_order`) - so does
# `plugins/rust`'s own bin/test targets, for the same reason; (2) that unit's
# Cargo fingerprint records those exact features; (3) a real `g-mesh mcp-shim`
# process, spoken to over its actual NDJSON wire protocol with a hand-framed
# `initialize` + `tools/list`, returns a BYTE-IDENTICAL response - every tool
# schema's property order included - whether the binary was built scoped or
# workspace-wide. So today, core's shipped JSON key order does not actually
# diverge from what the workspace-wide test run exercises; `check_core_feature_
# isolation` below is the standing check that this stays true, since nothing
# about resolver v2's host/target separation is guaranteed by this script,
# only observed. `cargo test -p g-mesh` in ci.yml's Linux row (full runs, which
# this script's CI gate requires) is the general backstop
# underneath that specific check: it actually builds and runs core's own
# suite in the exact scope that ships, so a divergence of any kind - this
# one, or one a future dependency introduces that this narrow check does not
# anticipate - has to fail a real test to reach a release.
#
# GM-374: this script has tagged `main` after every merge since it was
# written - the premise "add tagging so this can't recur" was already true.
# What actually happened is nobody RAN it: main carried twelve consecutive
# `merge: release-X into main` commits, 2.11.0 through 3.8.0, with no tag
# anywhere near them, because the ritual depended on a human remembering to
# invoke a script that produces no visible harm when skipped - CI never
# reacts to a missing tag, it only reacts to one being pushed. A tool that
# is only correct when run is not a fix for "nobody ran it".
#
# check_no_untagged_prior_releases below is what makes a second lapse
# structural instead of just possible-to-avoid: before tagging the release
# this invocation is cutting, it walks main's first-parent history BEHIND
# that commit and refuses if an earlier `merge: release-X into main` commit
# has no `vX` tag anywhere in the repo. Next time a release ships without
# running this script, the release after THAT one cannot be cut until the
# gap is back-filled - the omission becomes a blocked release instead of a
# silent one. The walk stops at the first version that already has a tag
# (by name, not by which commit it points at), so it costs nothing once a
# gap is closed and never re-opens the four pre-2.11.0 tags that anchor to
# their release branch's tip rather than to the main-merge commit - that
# inconsistency is documented, not something this check is meant to police.
# Cost: a release can now be blocked by a PRIOR release's missed tag, not
# just its own state - which is the point, but it means back-filling is now
# sometimes mandatory before a release can ship at all, not merely tidy.
#
# ---------------------------------------------------------------------------
# WHAT IT REFUSES ON
#
# Every one of these is a refusal, not a warning, and each says what to do
# about it:
#   - not on `main`
#   - a dirty working tree
#   - `main` out of sync with its upstream (a commit only local, or only
#     remote, cannot be the commit CI tags when the push happens)
#   - the tag already exists, locally or on the remote
#   - an earlier `merge: release-X into main` commit has no tag at all
#     (GM-374 - see the comment above this section)
#   - `main`'s HEAD not being `merge: release-<version> into main`, or that
#     merge's release-side parent not being the tip of
#     `origin/release-<version>` with a successful ci.yml run (GM-391)
#   - a version string that is not `X.Y.Z`
#   - `core/Cargo.toml`'s version disagreeing with the argument
#   - any other workspace member's version disagreeing with core's (GM-288)
#   - plugins/rust's, plugins/python's or plugins/typescript's plugin.toml
#     `plugin_version` disagreeing with its own Cargo.toml version, or that
#     plugin having changed since the previous release tag without a version
#     bump, or its version being lower than at that tag
#   - plugins/go's plugin.toml `plugin_version` disagreeing with the Go
#     plugin's own `pluginVersion` const (GM-303)
#   - core's own normal dependency graph reaching serde_json's
#     `preserve_order` feature (GM-302) - see that comment above for why this
#     is checked directly instead of trusted to stay absent
#   - the test sections' coverage check failing (`scripts/test-sections.sh check`)
#   - `main` not being a `merge: release-<v> into main` merge whose tree equals
#     its release-side parent's tree, or origin/release-<v> not being that parent
#   - no green full ci.yml run on that parent (GM-675): none at all, only
#     partial runs, only failed/cancelled ones, or one still running when
#     --ci-timeout expires
# ---------------------------------------------------------------------------

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

die() {
	echo "cut-release: $*" >&2
	exit 1
}

log() {
	echo "==> $*"
}

usage() {
	cat <<'EOF'
usage: scripts/cut-release.sh <version> [--push] [--ci-timeout <minutes>]

  <version>      the release version, X.Y.Z, matching core/Cargo.toml
  --push         also push the tag (starts the build/publish workflow)
  --ci-timeout   minutes to wait for a running ci.yml run on the release tip
                 (default 90; 0 = do not wait)
EOF
}

# The other workspace members whose `[package] version` must agree with
# core's - see the GM-288 comment above this script's header for why this
# exists instead of workspace-level version inheritance. Kept as a flat list
# rather than derived from the root Cargo.toml's `members` array: deriving it
# would let a member opt out of the check by construction (any member listed
# there is, by definition, checked), whereas the point is that every member
# is checked, with a new one requiring a deliberate addition here - the same
# fail-closed shape `SUPPORTED_TARGETS` in build-targets.sh already uses for
# release targets.
OTHER_WORKSPACE_MANIFESTS=(
	"wire/Cargo.toml"
	"walk/Cargo.toml"
	"plugins/sdk/Cargo.toml"
)

# The `version` of a manifest's `[package]` section. Same restriction as
# build-targets.sh's own `crate_version` and for the same reason: `version =`
# also appears under `[dependencies]`.
package_version() {
	awk '
		/^\[/ { in_package = ($0 == "[package]") }
		in_package && /^version[[:space:]]*=/ {
			gsub(/[",]/, "", $3); print $3; exit
		}
	' "$1"
}

# Refuses if any other workspace member's own `[package] version` disagrees
# with `core/Cargo.toml`'s (already resolved as `version` by the caller).
# Collects every mismatch before dying, rather than stopping at the first,
# because a release that is about to fix one drifted crate wants to know
# about all of them in the same run rather than finding the second one after
# re-running this script.
check_workspace_versions() {
	local core_version="$1" manifest path mismatches=()
	for manifest in "${OTHER_WORKSPACE_MANIFESTS[@]}"; do
		path="$REPO_ROOT/$manifest"
		[ -f "$path" ] || die "workspace member manifest not found: $manifest (update OTHER_WORKSPACE_MANIFESTS in this script if it moved or was removed)"
		local member_version
		member_version="$(package_version "$path")"
		[ -n "$member_version" ] || die "could not determine version from $manifest"
		if [ "$member_version" != "$core_version" ]; then
			mismatches+=("$manifest says $member_version")
		fi
	done
	if [ ${#mismatches[@]} -gt 0 ]; then
		local line
		echo "cut-release: workspace version drift - core/Cargo.toml says $core_version, but:" >&2
		for line in "${mismatches[@]}"; do
			echo "  - $line" >&2
		done
		die "fix every listed manifest's [package] version to $core_version before tagging"
	fi
	log "workspace versions agree: $core_version (core, ${OTHER_WORKSPACE_MANIFESTS[*]})"
}

# The cargo-workspace language plugins, by directory under plugins/. Each
# carries its own version and is checked by `check_crate_backed_plugin_versions`
# (see this script's header comment for the rule, and why Go is not here). A
# new cargo-workspace plugin is a deliberate addition to this list, the same
# fail-closed shape OTHER_WORKSPACE_MANIFESTS uses.
CRATE_BACKED_PLUGINS=(
	rust
	python
	typescript
)

# The release-versioned crates every plugin in CRATE_BACKED_PLUGINS is built
# from. Their `src/` and `Cargo.toml` are shipped inputs of each plugin.
PLUGIN_SHARED_CRATES=(
	plugins/sdk
	wire
	walk
)

# The `plugin_version` of a plugin.toml's `[plugin]` section. Same
# restriction as `package_version` and for the same reason: scoping to the
# section it's declared in is what keeps this correct if the key ever
# appears elsewhere too.
plugin_toml_version() {
	awk '
		/^\[/ { in_plugin = ($0 == "[plugin]") }
		in_plugin && /^plugin_version[[:space:]]*=/ {
			gsub(/[",]/, "", $3); print $3; exit
		}
	' "$1"
}

# The `name` of a manifest's `[package]` section.
package_name() {
	awk '
		/^\[/ { in_package = ($0 == "[package]") }
		in_package && /^name[[:space:]]*=/ {
			gsub(/[",]/, "", $3); print $3; exit
		}
	' "$1"
}

# The newest `v*` tag reachable from HEAD other than `v$1` (the tag this run
# creates). Prints nothing when there is none, or when $REPO_ROOT is not a git
# repository.
previous_release_tag() {
	local current="v$1" tag
	while IFS= read -r tag; do
		[ "$tag" = "$current" ] && continue
		printf '%s\n' "$tag"
		return 0
	done < <(git -C "$REPO_ROOT" tag --merged HEAD -l 'v[0-9]*' --sort=-v:refname 2>/dev/null)
}

# The manifest on stdin (a Cargo.toml or a plugin.toml) without its own
# version line, `[package] version` or `[plugin] plugin_version`. Two copies
# that differ only by a version bump print the same; a `version` key in any
# other section is kept.
strip_version() {
	awk '
		/^\[/ { section = $0 }
		section == "[package]" && /^version[[:space:]]*=/ { next }
		section == "[plugin]" && /^plugin_version[[:space:]]*=/ { next }
		{ print }
	'
}

# `name version source` of every package the crate named $1 reaches in the
# Cargo.lock on stdin, sorted, for packages with a `source` (registry or git)
# only. Path packages (workspace members) have no `source`: they are walked
# through but never printed, so a member's version bump does not show up here.
# A dependency written without a version matches every package of that name.
# The lock does not tell dev-dependencies apart, so they are reached too.
# Prints nothing if the crate is not in the lock.
lock_closure() {
	awk -v root="$1" '
		function flush() {
			if (name != "") { n++; pname[n] = name; pver[n] = ver; psrc[n] = src; pdeps[n] = deps }
			name = ""; ver = ""; src = ""; deps = ""; in_deps = 0
		}
		function add_deps(text,    m, k, items, item) {
			m = split(text, items, ",")
			for (k = 1; k <= m; k++) {
				item = items[k]
				gsub(/"/, "", item); gsub(/\[/, "", item); gsub(/\]/, "", item)
				gsub(/^[[:space:]]+|[[:space:]]+$/, "", item)
				if (item != "") deps = deps "|" item
			}
		}
		/^\[\[package\]\]/ { flush(); in_pkg = 1; next }
		/^\[/ { flush(); in_pkg = 0; next }
		!in_pkg { next }
		in_deps {
			if ($0 ~ /^[[:space:]]*\]/) { in_deps = 0; next }
			add_deps($0); next
		}
		/^name[[:space:]]*=/ { name = $0; sub(/^[^"]*"/, "", name); sub(/".*$/, "", name); next }
		/^version[[:space:]]*=/ { ver = $0; sub(/^[^"]*"/, "", ver); sub(/".*$/, "", ver); next }
		/^source[[:space:]]*=/ { src = $0; sub(/^[^"]*"/, "", src); sub(/".*$/, "", src); next }
		/^dependencies[[:space:]]*=/ {
			rest = $0; sub(/^[^\[]*\[/, "", rest)
			if (rest ~ /\]/) { add_deps(rest) } else { add_deps(rest); in_deps = 1 }
			next
		}
		END {
			flush()
			for (i = 1; i <= n; i++) if (pname[i] == root) { seen[i] = 1; queue[++tail] = i }
			while (head < tail) {
				i = queue[++head]
				m = split(pdeps[i], d, "|")
				for (k = 1; k <= m; k++) {
					if (d[k] == "") continue
					split(d[k], f, " ")
					for (j = 1; j <= n; j++) {
						if (!(j in seen) && pname[j] == f[1] && (f[2] == "" || pver[j] == f[2])) {
							seen[j] = 1; queue[++tail] = j
						}
					}
				}
			}
			for (j in seen) if (psrc[j] != "") print pname[j], pver[j], psrc[j]
		}
	' | sort
}

# Prints the shipped inputs of plugins/$1 that differ between tag $2 and the
# working tree, one per line, and nothing when the plugin did not change. The
# shipped inputs are: the plugin's and PLUGIN_SHARED_CRATES' `src/`; the
# plugin's `Cargo.toml` and `plugin.toml` and PLUGIN_SHARED_CRATES'
# `Cargo.toml`, each without its own version line (`strip_version`); and the
# plugin's registry/git dependency closure in Cargo.lock (`lock_closure`). A
# manifest missing on one side and present on the other counts as changed.
# Nothing else does (tests/, conformance/, README.md, package.json, sdk's
# fixture binaries, ...).
plugin_changed_since() {
	local dir="plugins/$1" tag="$2" shared manifest crate
	local src_dirs=("$dir/src") manifests=("$dir/Cargo.toml" "$dir/plugin.toml")
	for shared in "${PLUGIN_SHARED_CRATES[@]}"; do
		src_dirs+=("$shared/src")
		manifests+=("$shared/Cargo.toml")
	done

	git -C "$REPO_ROOT" diff --name-only "$tag" -- "${src_dirs[@]}" || return 1

	for manifest in "${manifests[@]}"; do
		if ! cmp -s \
			<(git -C "$REPO_ROOT" show "$tag:$manifest" 2>/dev/null | strip_version) \
			<(strip_version < "$REPO_ROOT/$manifest" 2>/dev/null); then
			echo "$manifest"
		fi
	done

	crate="$(package_name "$REPO_ROOT/$dir/Cargo.toml")"
	if ! cmp -s \
		<(git -C "$REPO_ROOT" show "$tag:Cargo.lock" 2>/dev/null | lock_closure "$crate") \
		<(lock_closure "$crate" < "$REPO_ROOT/Cargo.lock" 2>/dev/null); then
		echo "Cargo.lock: $crate dependency closure"
	fi
}

# For each plugin in CRATE_BACKED_PLUGINS, refuses if
#   (a) its plugin.toml `plugin_version` differs from its Cargo.toml
#       `[package] version` (checked even without a previous tag, and under
#       an offline check), or, against the previous release tag
#       (`previous_release_tag "$1"`):
#   (b) its version is lower than at that tag, or
#   (c) it changed since that tag (`plugin_changed_since`) and its version
#       did not move.
# A plugin absent at the tag is new and passes (b) and (c); a bump without a
# change passes with a log line. With no previous tag, (b) and (c) are
# skipped with a log line. $1 is the release version, used only to skip its
# own tag. Collects every refusal before dying, same reasoning as
# `check_workspace_versions`.
check_crate_backed_plugin_versions() {
	local release_version="$1" plugin dir cargo_toml plugin_toml
	local crate_version toml_version prev_tag prev_version changed summary
	local refusals=() agreed=()

	prev_tag="$(previous_release_tag "$release_version")"
	if [ -z "$prev_tag" ]; then
		log "no earlier v* release tag reachable from HEAD: skipping the plugin change rule, checking plugin.toml/Cargo.toml agreement only"
	fi

	for plugin in "${CRATE_BACKED_PLUGINS[@]}"; do
		dir="plugins/$plugin"
		cargo_toml="$REPO_ROOT/$dir/Cargo.toml"
		plugin_toml="$REPO_ROOT/$dir/plugin.toml"
		[ -f "$cargo_toml" ] || die "plugin manifest not found: $dir/Cargo.toml (update CRATE_BACKED_PLUGINS in this script if it moved or was removed)"
		[ -f "$plugin_toml" ] || die "plugin manifest not found: $dir/plugin.toml (update CRATE_BACKED_PLUGINS in this script if it moved or was removed)"
		crate_version="$(package_version "$cargo_toml")"
		[ -n "$crate_version" ] || die "could not determine version from $dir/Cargo.toml"
		toml_version="$(plugin_toml_version "$plugin_toml")"
		[ -n "$toml_version" ] || die "could not determine plugin_version from $dir/plugin.toml"

		if [ "$toml_version" != "$crate_version" ]; then
			refusals+=("$dir/plugin.toml says $toml_version, $dir/Cargo.toml says $crate_version - they must agree")
			continue
		fi
		agreed+=("$dir/plugin.toml $crate_version")

		[ -n "$prev_tag" ] || continue
		if ! git -C "$REPO_ROOT" cat-file -e "$prev_tag:$dir/Cargo.toml" 2>/dev/null; then
			log "$dir is new since $prev_tag, starts at $crate_version"
			continue
		fi
		prev_version="$(git -C "$REPO_ROOT" show "$prev_tag:$dir/Cargo.toml" | package_version -)"
		[ -n "$prev_version" ] || die "could not determine version from $dir/Cargo.toml at $prev_tag"

		if [ "$crate_version" != "$prev_version" ] &&
			[ "$(printf '%s\n%s\n' "$prev_version" "$crate_version" | sort -V | head -n 1)" = "$crate_version" ]; then
			refusals+=("$dir went backwards: $crate_version now, $prev_version at $prev_tag")
			continue
		fi

		changed="$(plugin_changed_since "$plugin" "$prev_tag")" ||
			die "could not compare $dir with $prev_tag"
		if [ -z "$changed" ]; then
			if [ "$crate_version" = "$prev_version" ]; then
				log "$dir unchanged since $prev_tag, stays $crate_version"
			else
				log "$dir bumped $prev_version -> $crate_version without a change since $prev_tag"
			fi
		elif [ "$crate_version" = "$prev_version" ]; then
			summary="$(printf '%s\n' "$changed" | head -n 3 | paste -s -d ',' - | sed 's/,/, /g')"
			if [ "$(printf '%s\n' "$changed" | wc -l)" -gt 3 ]; then
				summary="$summary, ..."
			fi
			refusals+=("$dir changed since $prev_tag ($summary) but its version is still $crate_version - bump $dir/Cargo.toml and $dir/plugin.toml")
		else
			log "$dir changed since $prev_tag, bumped $prev_version -> $crate_version"
		fi
	done

	if [ ${#refusals[@]} -gt 0 ]; then
		local line
		echo "cut-release: workspace plugin version check failed:" >&2
		for line in "${refusals[@]}"; do
			echo "  - $line" >&2
		done
		die "fix every listed plugin's Cargo.toml and plugin.toml version before tagging"
	fi
	log "crate-backed plugin_version agrees with each crate: ${agreed[*]}"
}

# Refuses if plugins/go's plugin.toml `plugin_version` disagrees with the Go
# plugin's own `pluginVersion` const - the self-versioned half of GM-303's
# rule. Deliberately NOT compared against `$core_version`: plugins/go is not a
# cargo workspace member and its version tracks the plugin's own history, not
# the release train (see this script's header comment).
check_self_versioned_plugin_versions() {
	local mismatches=()

	local go_manifest_version go_const_version
	go_manifest_version="$(plugin_toml_version "$REPO_ROOT/plugins/go/plugin.toml")"
	[ -n "$go_manifest_version" ] || die "could not determine plugin_version from plugins/go/plugin.toml"
	go_const_version="$(grep -E -m1 '^[[:space:]]*pluginVersion[[:space:]]*=' "$REPO_ROOT/plugins/go/wire.go" | sed -E 's/.*"([^"]+)".*/\1/')"
	[ -n "$go_const_version" ] || die "could not determine pluginVersion from plugins/go/wire.go"
	if [ "$go_manifest_version" != "$go_const_version" ]; then
		mismatches+=("plugins/go/plugin.toml says $go_manifest_version, plugins/go/wire.go says $go_const_version")
	fi

	if [ ${#mismatches[@]} -gt 0 ]; then
		local line
		echo "cut-release: plugin_version disagrees with the plugin's own manifest of record:" >&2
		for line in "${mismatches[@]}"; do
			echo "  - $line" >&2
		done
		die "fix the listed plugin.toml (or its counterpart) so the two agree before tagging"
	fi
	log "self-versioned plugin_version agrees with its own manifest: go $go_manifest_version"
}

# GM-302: refuses if core's own NORMAL (non-build, non-proc-macro) dependency
# graph reaches serde_json's `preserve_order` feature - see this script's
# header comment for the full investigation. `-e normal,features,
# no-proc-macro` is the edge-kind filter that excludes precisely the two host
# contexts (tree-sitter's build-dependency, rmcp-macros' proc-macro) that
# today carry `preserve_order` into `cargo tree -e features`'s default,
# all-edge-kinds reading without ever reaching a linked artifact - so this is
# the command that actually answers "does core's shipped JSON key order
# differ", not the broader one. Cheap and fast on purpose (no build, just
# resolver output) so it runs before the ten-plus-minute test suite rather
# than after it.
#
# Demonstrated to have teeth, not just to pass today: temporarily adding
# `features = ["preserve_order"]` to core/Cargo.toml's own `serde_json` line
# flips this command's count from 0 to 1 and this function from passing to
# dying - see GM-302's completion notes for the transcript. Reverted before
# that commit; core's `serde_json` line is untouched by this task.
check_core_feature_isolation() {
	local runtime_hits
	runtime_hits="$(cargo tree -p g-mesh -e normal,features,no-proc-macro 2>/dev/null | grep -c preserve_order || true)"
	if [ "$runtime_hits" -ne 0 ]; then
		die "core's own normal dependency graph now reaches serde_json's preserve_order feature ($runtime_hits edge(s) via 'cargo tree -p g-mesh -e normal,features,no-proc-macro') - core's shipped JSON key order would differ from what the workspace test sections exercise; see this script's GM-302 header comment before proceeding"
	fi
	log "core's runtime dependency graph does not reach preserve_order (GM-302): cargo tree -p g-mesh -e normal,features,no-proc-macro -> 0"

	# Informational only, never a failure: the raw, all-edge-kinds count
	# `cargo tree -e features` reports by default (what GM-302's own
	# investigation started from, and what plugins/rust/Cargo.toml's and
	# plugins/python/Cargo.toml's own comments point at) still disagrees
	# between the scoped and workspace views. Logged so a reader who only ran
	# the bare commands from those comments is not left thinking this
	# function missed something - the check above already confirmed that
	# specific disagreement is host-only and does not reach anything linked.
	local scoped_all workspace_all
	scoped_all="$(cargo tree -p g-mesh -e features 2>/dev/null | grep -c preserve_order || true)"
	workspace_all="$(cargo tree -e features 2>/dev/null | grep -c preserve_order || true)"
	if [ "$scoped_all" != "$workspace_all" ]; then
		log "note: cargo tree -e features (all edge kinds, cargo's default) still disagrees - $scoped_all (scoped) vs $workspace_all (workspace); confirmed host-only (tree-sitter's build-dependency, rmcp-macros' proc-macro) by the runtime-scoped check above, so this is not gated on"
	fi
}

# GM-374: refuses if main's first-parent history carries an earlier
# `merge: release-X into main` commit with no `vX` tag anywhere in the repo.
# See this script's GM-374 header comment for why this exists and what it
# costs. Walks backward from `main^` (the commit BEFORE the one this
# invocation is about to tag) and stops at the first version that already
# has a tag by name - it does not check that the tag points at that exact
# commit, because the pre-2.11.0 tags deliberately don't (v2.8.0, v2.8.1 and
# v2.9.0 anchor to their release branch's tip; only v2.10.0 onward anchors
# to the main-merge commit) and re-litigating that is not this check's job.
# A merge commit whose subject doesn't match the `merge: release-X into
# main` convention exactly is invisible to this walk, same as it would be to
# a person reading `git log --first-parent` for the same pattern - this is a
# safety net for the established convention, not a guarantee independent of
# it.
check_no_untagged_prior_releases() {
	local head_parent
	head_parent="$(git -C "$REPO_ROOT" rev-parse main^ 2>/dev/null)" || return 0

	local missing=()
	local line sha subject ver
	while IFS= read -r line; do
		sha="${line%% *}"
		subject="${line#* }"
		if [[ "$subject" =~ ^merge:\ release-([0-9]+\.[0-9]+\.[0-9]+)\ into\ main$ ]]; then
			ver="${BASH_REMATCH[1]}"
			if [ -z "$(git -C "$REPO_ROOT" tag -l "v$ver")" ]; then
				missing+=("v$ver ($sha)")
			else
				break
			fi
		fi
	done < <(git -C "$REPO_ROOT" log --first-parent --format="%H %s" "$head_parent")

	if [ ${#missing[@]} -gt 0 ]; then
		local line2
		echo "cut-release: earlier release(s) on main were never tagged:" >&2
		for line2 in "${missing[@]}"; do
			echo "  - $line2" >&2
		done
		die "back-fill the tag(s) above first (git tag -a v<version> <sha> -m 'g-mesh v<version>') - this is exactly how the tag history went missing from v2.10.0 to v3.8.0 (GM-374)"
	fi
	log "no untagged prior releases behind $(git -C "$REPO_ROOT" rev-parse --short main^) on main's first-parent history"
}

# owner/repo parsed from the `origin` remote, for printing real URLs at the
# end rather than a placeholder someone has to mentally substitute.
origin_slug() {
	local url
	url="$(git -C "$REPO_ROOT" remote get-url origin 2>/dev/null)" || return 1
	# Handles both git@github.com:owner/repo.git and https://github.com/owner/repo.git
	url="${url%.git}"
	url="${url#git@github.com:}"
	url="${url#https://github.com/}"
	url="${url#http://github.com/}"
	printf '%s\n' "$url"
}

# GM-391 / GM-675: the release-side parent of `merge: release-<version> into
# main` (T = M^2) must be the tip of `origin/release-<version>`, and M's tree
# must equal T's tree (main has nothing the release branch lacks), so a green
# CI run on T tested exactly the code the tag will carry. Cheap, no network but
# one ls-remote; split from the wait so every cheap check can fail first.
#
# `merge_rev` defaults to `main`; it is a parameter so this can be checked
# against an earlier release's merge commit by sourcing this file.
check_release_merge_shape() {
	local version="$1" merge_rev="${2:-main}"
	local branch="release-$version"

	local subject
	subject="$(git -C "$REPO_ROOT" log -1 --format=%s "$merge_rev")"
	[ "$subject" = "merge: $branch into main" ] ||
		die "$merge_rev is '$subject', not 'merge: $branch into main' - merge the release branch into main with that subject before cutting $version"

	local tip
	tip="$(git -C "$REPO_ROOT" rev-parse --verify --quiet "$merge_rev^2")" ||
		die "$merge_rev has no second parent - merge $branch with --no-ff so the tested release commit stays identifiable"

	local slug
	slug="$(origin_slug)" || die "could not read the origin remote's GitHub owner/repo"

	local remote_tip
	remote_tip="$(git -C "$REPO_ROOT" ls-remote origin "refs/heads/$branch" | cut -f1)" ||
		die "could not list $branch on origin - check network/remote access and try again"
	[ -n "$remote_tip" ] ||
		die "$branch was never pushed, so ci.yml never ran on it - push it ('git push origin $branch'), run 'gh workflow run ci.yml --ref $branch', wait for a green run (https://github.com/$slug/actions?query=branch%3A$branch), then re-run this"
	[ "$remote_tip" = "$tip" ] ||
		die "origin/$branch is at ${remote_tip:0:7}, but main merged ${tip:0:7} - main does not contain what was pushed and tested; reconcile the two before tagging"

	local merge_tree tip_tree
	merge_tree="$(git -C "$REPO_ROOT" rev-parse "$merge_rev^{tree}")"
	tip_tree="$(git -C "$REPO_ROOT" rev-parse "$tip^{tree}")"
	[ "$merge_tree" = "$tip_tree" ] ||
		die "main has commits that are not on $branch (the tree of $merge_rev differs from the tree of ${tip:0:7}) - merge main into $branch, push, let CI pass, merge again"
}

# The name of ci.yml's `full-suite` job. Must stay equal to ci.yml's job name
# and to CI_FULL_SUITE_JOB in scripts/ci-green-for-commit.sh (GM-613), which
# asks release.yml's question with the same query.
CI_FULL_SUITE_JOB="Full test suite ran"

# GM-675: the single place cut-release calls GitHub. $GH replaces
# the `gh` binary (tests point it at a fake; a fake `gh` on PATH works too).
cut_release_gh() {
	"${GH:-gh}" "$@"
}

# One query of ci.yml's runs for T, with a retry: prints one line per run,
# "<id> <status> <conclusion|none> <attempt>", filtered by head_sha. Three
# consecutive failures die; an error never counts as green.
ci_runs_for_sha() {
	local slug="$1" sha="$2" out="" try
	for try in 1 2 3; do
		if out="$(cut_release_gh api "repos/$slug/actions/workflows/ci.yml/runs?head_sha=$sha&per_page=100" \
			--jq ".workflow_runs[] | select(.head_sha == \"$sha\") | \"\(.id) \(.status) \(.conclusion // \"none\") \(.run_attempt)\"")"; then
			printf '%s\n' "$out"
			return 0
		fi
		[ "$try" -eq 3 ] || sleep "${CUT_RELEASE_CI_POLL_SECS:-60}"
	done
	die "could not query ci.yml runs for ${sha:0:7} after 3 attempts (is 'gh auth status' logged in?)"
}

# 1 if run $2's latest attempt has a successful "$CI_FULL_SUITE_JOB" job.
ci_run_has_full_suite() {
	local slug="$1" run_id="$2" n="" try
	for try in 1 2 3; do
		if n="$(cut_release_gh api "repos/$slug/actions/runs/$run_id/jobs?filter=latest&per_page=100" \
			--jq "[.jobs[] | select(.name == \"$CI_FULL_SUITE_JOB\" and .conclusion == \"success\")] | length")"; then
			[ "$n" -gt 0 ] && echo 1 || echo 0
			return 0
		fi
		[ "$try" -eq 3 ] || sleep "${CUT_RELEASE_CI_POLL_SECS:-60}"
	done
	die "could not query the jobs of ci.yml run $run_id after 3 attempts (is 'gh auth status' logged in?)"
}

# GM-675: wait for a green full ci.yml run on T = merge_rev^2, then return.
# States, in this order: green (a completed/success run whose full-suite job
# succeeded: return), running (any run not completed: poll again), partial
# (green runs without the full-suite job: die, dispatch hint), failed (only
# completed non-success runs: die naming them), missing (no run: die).
# A green full run wins over any other run on the same SHA (a red run followed
# by a green one is a flake someone re-ran).
wait_for_green_full_ci() {
	local version="$1" merge_rev="${2:-main}" timeout_min="${3:-90}"
	local branch="release-$version" poll="${CUT_RELEASE_CI_POLL_SECS:-60}"

	command -v "${GH:-gh}" >/dev/null 2>&1 ||
		die "gh (GitHub CLI) not found - it is needed to confirm ci.yml passed on $branch"

	local tip slug
	tip="$(git -C "$REPO_ROOT" rev-parse "$merge_rev^2")"
	slug="$(origin_slug)" || die "could not read the origin remote's GitHub owner/repo"

	local start now elapsed_min
	start="$(date +%s)"
	while :; do
		local runs id status conclusion attempt
		runs="$(ci_runs_for_sha "$slug" "$tip")" || exit 1

		local running_id="" running_desc="" partial=0 failed_desc="" any=0 cancelled=0
		while read -r id status conclusion attempt; do
			[ -n "$id" ] || continue
			any=1
			if [ "$status" != "completed" ]; then
				[ -n "$running_id" ] || {
					running_id="$id"
					running_desc="$status, attempt $attempt"
				}
			elif [ "$conclusion" = "success" ]; then
				local full
				full="$(ci_run_has_full_suite "$slug" "$id")" || exit 1
				if [ "$full" = "1" ]; then
					log "$branch at ${tip:0:7} is on origin and passed a full ci.yml run ($id)"
					return 0
				fi
				partial=1
			else
				[ "$conclusion" != "cancelled" ] || cancelled=1
				failed_desc="$failed_desc
  run $id: $conclusion (attempt $attempt) https://github.com/$slug/actions/runs/$id"
			fi
		done <<<"$runs"

		now="$(date +%s)"
		elapsed_min=$(((now - start) / 60))
		if [ -n "$running_id" ]; then
			if [ "$elapsed_min" -ge "$timeout_min" ]; then
				die "ci.yml run $running_id for $branch at ${tip:0:7} is still running after ${timeout_min}m (https://github.com/$slug/actions/runs/$running_id) - nothing was tagged; re-run this to keep waiting (--ci-timeout)"
			fi
			log "waiting for ci.yml run $running_id ($running_desc), ${elapsed_min}m of ${timeout_min}m"
			sleep "$poll"
			continue
		fi
		if [ "$partial" -eq 1 ]; then
			die "ci.yml passed on $branch at ${tip:0:7}, but only as a partial run (test sections selected by changed paths) - run every section: 'gh workflow run ci.yml --ref $branch', wait for it to pass (https://github.com/$slug/actions/workflows/ci.yml?query=branch%3A$branch), then re-run this"
		fi
		if [ -n "$failed_desc" ]; then
			local note=""
			[ "$cancelled" -eq 0 ] || note="
a newer push to $branch cancels a running run (ci.yml concurrency)"
			die "no green full ci.yml run for $branch at ${tip:0:7}; failed runs:$failed_desc$note
fix it on $branch and push, or re-run the failed run, then re-run this"
		fi
		[ "$any" -eq 1 ] ||
			die "no ci.yml run for $branch at ${tip:0:7} - push it ('git push origin $branch'), run 'gh workflow run ci.yml --ref $branch', wait for a green run (https://github.com/$slug/actions?query=branch%3A$branch), then re-run this; main has not been verified by CI"
	done
}

main() {
	local version="" push=0 ci_timeout=90
	while [ $# -gt 0 ]; do
		case "$1" in
		--push)
			push=1
			;;
		--skip-tests)
			die "--skip-tests is gone (GM-675): cut-release runs no test suite; CI on release-<v> is the gate"
			;;
		--ci-timeout)
			[ $# -ge 2 ] || die "--ci-timeout needs a number of minutes"
			ci_timeout="$2"
			[[ "$ci_timeout" =~ ^[0-9]+$ ]] || die "--ci-timeout takes whole minutes, got '$ci_timeout'"
			shift
			;;
		-h | --help)
			usage
			return 0
			;;
		-*)
			die "unknown flag: $1 (try --help)"
			;;
		*)
			[ -z "$version" ] || die "unexpected extra argument: $1"
			version="$1"
			;;
		esac
		shift
	done

	[ -n "$version" ] || {
		usage >&2
		die "missing <version> argument"
	}

	[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
		die "malformed version: '$version' (expected X.Y.Z, three dot-separated numbers, e.g. 2.7.0)"

	local tag="v$version"

	local branch
	branch="$(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD)"
	[ "$branch" = "main" ] ||
		die "not on main (currently on '$branch') - run 'git checkout main' first"

	[ -z "$(git -C "$REPO_ROOT" status --porcelain)" ] ||
		die "working tree is dirty - commit, stash, or discard changes before cutting a release (see 'git status')"

	log "fetching origin to check main is in sync"
	git -C "$REPO_ROOT" fetch --quiet origin main ||
		die "could not fetch origin/main - check network/remote access and try again"

	local local_sha upstream_sha
	local_sha="$(git -C "$REPO_ROOT" rev-parse main)"
	upstream_sha="$(git -C "$REPO_ROOT" rev-parse origin/main)"
	if [ "$local_sha" != "$upstream_sha" ]; then
		local ahead behind
		ahead="$(git -C "$REPO_ROOT" rev-list --count origin/main..main)"
		behind="$(git -C "$REPO_ROOT" rev-list --count main..origin/main)"
		if [ "$ahead" -gt 0 ]; then
			die "main is $ahead commit(s) ahead of origin/main - push it first ('git push origin main'); tagging a commit CI can never see produces a tag nobody can build"
		else
			die "main is $behind commit(s) behind origin/main - pull first ('git pull origin main')"
		fi
	fi

	[ -z "$(git -C "$REPO_ROOT" tag -l "$tag")" ] ||
		die "tag $tag already exists locally - delete it first ('git tag -d $tag') if you mean to recut it"

	[ -z "$(git -C "$REPO_ROOT" ls-remote --tags origin "refs/tags/$tag")" ] ||
		die "tag $tag already exists on origin - a release for $version has already been cut"

	# GM-374: refuses if an earlier release shipped without this script
	# tagging it - see the GM-374 header comment and the function itself.
	check_no_untagged_prior_releases

	# GM-391/GM-675: the release branch must have been pushed, and main's merge
	# must carry exactly its tree - see the function's own comment. The wait for
	# CI itself is the last step before tagging.
	check_release_merge_shape "$version"

	# The exact check .github/workflows/release.yml's "Check the tag matches
	# the crate version" step runs, so a mismatch is caught here instead of
	# costing four ~90-minute builds. --version needs no toolchain.
	local crate_version
	crate_version="$(bash "$REPO_ROOT/scripts/build-targets.sh" --version)"
	[ "$crate_version" = "$version" ] ||
		die "core/Cargo.toml says $crate_version, not $version - the release branch's first commit should have bumped it; fix core/Cargo.toml (or pass the version that's actually there) before tagging"

	# GM-288 (GM-298 added a fourth): core/Cargo.toml agreeing with the tag is
	# not enough on its own now that four more workspace members carry their
	# own hand-pinned version - see this script's header comment for why this
	# is a second check here rather than workspace-level inheritance.
	check_workspace_versions "$crate_version"

	# GM-303: plugin.toml's `plugin_version`, checked by two different rules
	# for two different reasons - see this script's header comment.
	check_crate_backed_plugin_versions "$crate_version"
	check_self_versioned_plugin_versions

	# GM-302: cheap (no build) and fast, so it runs before the ten-plus-minute
	# suite below rather than after it - see this script's header comment and
	# the function's own for the full investigation.
	check_core_feature_isolation

	# The sections partition the suite (`check`): a test in no section or in two
	# would be a hole in what CI runs. It builds the test binaries (a few
	# minutes), needs no plugin node_modules.
	log "checking that the test sections cover the whole suite"
	"$REPO_ROOT/scripts/test-sections.sh" check ||
		die "scripts/test-sections.sh check failed - a test is in no section or in two; fix the filtersets before cutting a release"

	# GM-675: the suite itself ran on GitHub CI (see the header comment).
	wait_for_green_full_ci "$version" main "$ci_timeout"

	log "tagging $tag on $(git -C "$REPO_ROOT" rev-parse --short main)"
	git -C "$REPO_ROOT" tag -a "$tag" -m "g-mesh $tag" ||
		die "failed to create tag $tag"

	if [ "$push" -eq 1 ]; then
		log "pushing $tag to origin"
		git -C "$REPO_ROOT" push origin "$tag" ||
			die "failed to push $tag - the tag still exists locally; retry with 'git push origin $tag'"
	else
		log "tag $tag created locally. Push it when ready:"
		echo
		echo "  git push origin $tag"
		echo
	fi

	local slug
	slug="$(origin_slug)" || slug="<owner>/<repo>"

	echo
	log "remaining steps"
	if [ "$push" -eq 1 ]; then
		echo "  1. Watch the build: https://github.com/$slug/actions"
		echo "  2. Check the published release: https://github.com/$slug/releases/tag/$tag"
	else
		echo "  1. Push the tag:    git push origin $tag"
		echo "  2. Watch the build: https://github.com/$slug/actions"
		echo "  3. Check the published release: https://github.com/$slug/releases/tag/$tag"
	fi
}

# GM-374: guarded so this file can be sourced (e.g. to call
# check_no_untagged_prior_releases directly, as this script's own tests do)
# without also running the full release ritual. A normal invocation
# (`scripts/cut-release.sh <version>`) is unaffected - BASH_SOURCE[0] equals
# $0 exactly when the file is executed rather than sourced.
if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
	main "$@"
fi
