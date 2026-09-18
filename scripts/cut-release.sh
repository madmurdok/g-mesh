#!/usr/bin/env bash
#
# One command to validate, test and tag a release, instead of the four manual
# steps described in .github/workflows/release.yml's header (check the crate
# version, run the workflow once by hand, tag, approve the draft). The version
# is typed here exactly once.
#
#   scripts/cut-release.sh <version>              # verify, test, tag locally
#   scripts/cut-release.sh <version> --push       # ...and push the tag
#   scripts/cut-release.sh <version> --skip-tests # skip the cargo test runs (--workspace and -p g-mesh)
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
# since GM-298, `plugins/python/Cargo.toml` each carry their own. Nothing
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
# stays the one manifest whose version *names* the release, and every other
# member's version is checked to equal it, in `check_workspace_versions`
# below, before a single test runs. A member added to the workspace later
# that forgets this gets caught here, not after a release ships with it
# silently stale - which is the whole reason this got written down instead of
# only fixed: this decision is what the next language plugin's manifest
# should follow too.
#
# GM-303: `plugin.toml`'s own `plugin_version` - a different field from every
# manifest `[package] version` above, and checked by a different rule, not
# the same one repeated. `g-mesh plugins list` prints it and core names it in
# a protocol-mismatch message, so whichever rule applies to a given plugin is
# the number a person reads first about it - and the four bundled plugins do
# not all follow the same one:
#
#   - plugins/rust and plugins/python ARE cargo workspace members (their
#     Cargo.toml is already in OTHER_WORKSPACE_MANIFESTS above), and each has
#     its own test forcing `plugin_version` to equal ITS crate's version
#     (`the_manifest_version_matches_the_crates`, added by GM-290 and GM-299
#     after finding it had drifted - plugins/rust/plugin.toml still said
#     0.1.0 three releases after the crate reached 3.3.0). Chained with the
#     check above, that makes `plugin_version` the release version for these
#     two, transitively - so `check_crate_backed_plugin_versions` below
#     checks it directly against `$crate_version`, the same way
#     `check_workspace_versions` does for the Cargo manifests.
#   - plugins/go and plugins/typescript are NOT workspace members - a
#     separate Go module and a separate npm package, each bumped by hand for
#     the plugin's OWN capability changes and nothing else
#     (plugins/typescript/package.json: 2.0.0 -> 2.1.0 for becoming a
#     self-contained Node SEA, -> 2.2.0 for the --run-node entry path;
#     plugins/go/wire.go's `pluginVersion` const: 0.1.0 -> 0.2.0 for the
#     go/types semantic tier). Forcing these to the release version would
#     make the number lie - g-mesh 3.5.0 shipped no change to the Go plugin
#     at all - so what `check_self_versioned_plugin_versions` below checks
#     instead is INTERNAL agreement: plugin.toml's copy against that
#     plugin's own manifest of record (package.json's `.version`, or the
#     `pluginVersion` const). Each already has a test for this too
#     (`every_declaration_of_the_bundled_plugins_version_agrees` in
#     core/src/daemon/manifest.rs; `TestPluginVersionMatchesTheManifest` in
#     plugins/go/manifest_test.go) - this script re-checks it directly by
#     reading the files, so drift is still caught under --skip-tests, and for
#     Go, without a Go toolchain on PATH (`go test` cannot run at all then).
#
# docs/architecture/plugin-modularity.md ("plugin_version: two rules, not
# one") is where a plugin author reads this before adding a fifth plugin;
# this comment is the enforcement side of that same decision.
#
# It tags `main`, and only after the release branch has already been merged
# into it - the tag is meant to point at the code someone gets by cloning, not
# at a branch tip nobody else can see. It does not create the release branch,
# does not merge anything, does not publish the draft Release GitHub builds
# (that stays a human decision, per #67's design), and does not touch the
# task tracker.
#
# Pushing the tag starts a public four-platform build and a draft Release, so
# it takes the explicit --push above; without it, the tag is created locally
# and the exact command to push it is printed instead.
#
# `cargo test --workspace` takes 10+ minutes on this machine. Skipping the announcement of
# that fact makes the wait look like a hang, so this script says what it is
# doing before it goes quiet. --skip-tests exists for re-running this script
# after a preflight check fails post-test (e.g. to fix --push without paying
# for the suite twice) - use it deliberately, not as a default habit.
#
# GM-302: `cargo test --workspace` above is GM-288's deliberate choice, but a
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
# diverge from what `cargo test --workspace` exercises; `check_core_feature_
# isolation` below is the standing check that this stays true, since nothing
# about resolver v2's host/target separation is guaranteed by this script,
# only observed. `cargo test -p g-mesh` further down is the general backstop
# underneath that specific check: it actually builds and runs core's own
# suite in the exact scope that ships, so a divergence of any kind - this
# one, or one a future dependency introduces that this narrow check does not
# anticipate - has to fail a real test to reach a release.
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
#   - a version string that is not `X.Y.Z`
#   - `core/Cargo.toml`'s version disagreeing with the argument
#   - any other workspace member's version disagreeing with core's (GM-288)
#   - plugins/rust's or plugins/python's plugin.toml `plugin_version`
#     disagreeing with core's version (GM-303)
#   - plugins/go's or plugins/typescript's plugin.toml `plugin_version`
#     disagreeing with that plugin's own manifest of record (GM-303)
#   - core's own normal dependency graph reaching serde_json's
#     `preserve_order` feature (GM-302) - see that comment above for why this
#     is checked directly instead of trusted to stay absent
#   - `cargo test --workspace` failing, OR `cargo test -p g-mesh` failing
#     (GM-302 added the second one; GM-288's comment on the first explains why
#     both run rather than either replacing the other)
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
usage: scripts/cut-release.sh <version> [--push] [--skip-tests]

  <version>      the release version, X.Y.Z, matching core/Cargo.toml
  --push         also push the tag (starts the build/publish workflow)
  --skip-tests   skip the cargo test runs (--workspace and -p g-mesh, GM-302) before tagging
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
	"plugins/sdk/Cargo.toml"
	"plugins/rust/Cargo.toml"
	"plugins/python/Cargo.toml"
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

# GM-303: bundled plugins whose plugin.toml `plugin_version` must equal the
# release version because the crate it's built from already does (see this
# script's header comment for the full rule and why Go/TypeScript are not
# here). A new cargo-workspace plugin's manifest is a deliberate addition to
# this list, the same fail-closed shape OTHER_WORKSPACE_MANIFESTS uses.
CRATE_BACKED_PLUGIN_MANIFESTS=(
	"plugins/rust/plugin.toml"
	"plugins/python/plugin.toml"
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

# Refuses if plugins/rust's or plugins/python's plugin.toml `plugin_version`
# disagrees with `$core_version` - the release-tracking half of GM-303's
# rule. Collects every mismatch before dying, same reasoning as
# `check_workspace_versions`.
check_crate_backed_plugin_versions() {
	local core_version="$1" manifest path mismatches=()
	for manifest in "${CRATE_BACKED_PLUGIN_MANIFESTS[@]}"; do
		path="$REPO_ROOT/$manifest"
		[ -f "$path" ] || die "plugin manifest not found: $manifest (update CRATE_BACKED_PLUGIN_MANIFESTS in this script if it moved or was removed)"
		local plugin_version
		plugin_version="$(plugin_toml_version "$path")"
		[ -n "$plugin_version" ] || die "could not determine plugin_version from $manifest"
		if [ "$plugin_version" != "$core_version" ]; then
			mismatches+=("$manifest says $plugin_version")
		fi
	done
	if [ ${#mismatches[@]} -gt 0 ]; then
		local line
		echo "cut-release: plugin_version drift - core/Cargo.toml says $core_version, but:" >&2
		for line in "${mismatches[@]}"; do
			echo "  - $line" >&2
		done
		die "fix every listed plugin.toml's plugin_version to $core_version before tagging"
	fi
	log "crate-backed plugin_version agrees: $core_version (${CRATE_BACKED_PLUGIN_MANIFESTS[*]})"
}

# Refuses if plugins/typescript's or plugins/go's plugin.toml
# `plugin_version` disagrees with that plugin's own manifest of record - the
# self-versioned half of GM-303's rule. Deliberately NOT compared against
# `$core_version`: these two are not cargo workspace members and their
# version tracks the plugin's own history, not the release train (see this
# script's header comment).
check_self_versioned_plugin_versions() {
	local mismatches=()

	local ts_manifest_version ts_package_version
	ts_manifest_version="$(plugin_toml_version "$REPO_ROOT/plugins/typescript/plugin.toml")"
	[ -n "$ts_manifest_version" ] || die "could not determine plugin_version from plugins/typescript/plugin.toml"
	ts_package_version="$(grep -m1 '"version"' "$REPO_ROOT/plugins/typescript/package.json" | sed -E 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')"
	[ -n "$ts_package_version" ] || die "could not determine .version from plugins/typescript/package.json"
	if [ "$ts_manifest_version" != "$ts_package_version" ]; then
		mismatches+=("plugins/typescript/plugin.toml says $ts_manifest_version, plugins/typescript/package.json says $ts_package_version")
	fi

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
	log "self-versioned plugin_version agrees with its own manifest: typescript $ts_manifest_version, go $go_manifest_version"
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
		die "core's own normal dependency graph now reaches serde_json's preserve_order feature ($runtime_hits edge(s) via 'cargo tree -p g-mesh -e normal,features,no-proc-macro') - core's shipped JSON key order would differ from what cargo test --workspace exercises; see this script's GM-302 header comment before proceeding"
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

main() {
	local version="" push=0 skip_tests=0
	while [ $# -gt 0 ]; do
		case "$1" in
		--push)
			push=1
			;;
		--skip-tests)
			skip_tests=1
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

	if [ "$skip_tests" -eq 1 ]; then
		log "skipping cargo test (--skip-tests)"
	else
		log "running cargo test --workspace - this takes 10+ minutes on this machine, not hung, just slow"
		local test_start test_end
		test_start="$(date +%s)"
		# GM-288: `--workspace` from the repo root, not `cd core && cargo
		# test` - since GM-284 made the repository a cargo workspace, the
		# latter tests core alone and would gate a release on green tests
		# while shipping wire/, plugins/sdk, plugins/rust and plugins/python
		# untested. Every one of those now ships inside every release archive
		# (the Rust plugin since GM-288, the Python plugin since GM-298), so a
		# release gate that does not run their tests is not actually gating on
		# them.
		(cd "$REPO_ROOT" && cargo test --workspace) ||
			die "cargo test --workspace failed - fix the failure before cutting a release"
		test_end="$(date +%s)"
		log "cargo test --workspace passed in $((test_end - test_start))s"

		# GM-302: --workspace above is deliberate (GM-288's comment on it,
		# just above, explains why) and stays - this is IN ADDITION, not a
		# replacement. `check_core_feature_isolation` already confirmed core's
		# own linked serde_json is the same in both configurations today, but
		# that confirmation is a property of this repository's CURRENT
		# dependency graph, not a standing guarantee the way a type system
		# would give - a future dependency change could shift it without that
		# narrow check anticipating the new shape. This run is the general
		# backstop underneath it: it actually builds and tests core in the
		# exact package scope `build-targets.sh` ships (`cd core && cargo
		# build`, no `-p`, which is what `-p g-mesh` from the workspace root
		# resolves the same way - both exclude plugins/rust and
		# plugins/python from the graph entirely), so ANY divergence between
		# what ships and what `--workspace` above just tested - this one, or
		# a different one neither check above was written for - has to fail
		# a real test to reach a release.
		log "running cargo test -p g-mesh - the exact package scope build-targets.sh ships (GM-302)"
		test_start="$(date +%s)"
		(cd "$REPO_ROOT" && cargo test -p g-mesh) ||
			die "cargo test -p g-mesh failed - the shipped configuration itself is broken, even though cargo test --workspace passed; fix it before cutting a release (see GM-302)"
		test_end="$(date +%s)"
		log "cargo test -p g-mesh passed in $((test_end - test_start))s"
	fi

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
		echo "  2. Review and publish the draft release: https://github.com/$slug/releases"
	else
		echo "  1. Push the tag:    git push origin $tag"
		echo "  2. Watch the build: https://github.com/$slug/actions"
		echo "  3. Review and publish the draft release: https://github.com/$slug/releases"
	fi
}

main "$@"
