#!/usr/bin/env bash
# Runs the Rust plugin's Linux sandbox tests (`sandbox::landlock_ns` unit
# tests, `semantic::tests::a_partial_sandbox_is_admitted`) and every
# integration binary that starts the sandboxed semantic engine
# (`semantic_runs_no_project_code`, `indexing_writes_nothing_and_stays_offline`,
# `conformance`) in Docker, from a macOS or Linux host.
#
#   scripts/test-linux-sandbox-docker.sh [runs]
#
# Each pass runs `runs` times (default 1), stopping at the first failure:
#   1. as root, with `seccomp=unconfined` (the user namespace works);
#   2. as uid 1000, with `seccomp=unconfined`;
#   3. once as uid 1000 under Docker's default seccomp profile, where no
#      user namespace can be made: the tier test then expects `Failed`.
#      Only the sandbox tests run here, since a server needing the layers
#      cannot start.
# Tests needing Landlock print a skip line where the kernel lacks it (Docker
# Desktop's does); those lines are listed at the end.
#
# The worktree is mounted read-only; the build goes to a container-local
# volume. Environment: IMAGE (default rust:1-bookworm), VOLUME_PREFIX
# (default g-mesh-linux-sandbox), SRC (default this script's repository).
# Give another source tree (SRC) its own VOLUME_PREFIX: cargo decides
# freshness by mtime, so a build from one tree can pass for the other's.
set -euo pipefail

runs="${1:-1}"
image="${IMAGE:-rust:1-bookworm}"
prefix="${VOLUME_PREFIX:-g-mesh-linux-sandbox}"
src="${SRC:-$(cd "$(dirname "$0")/.." && pwd)}"
tag="g-mesh-linux-sandbox-test:$(printf '%s' "$image" | tr ':/' '--')"
target_volume="$prefix-target"
registry_volume="$prefix-registry"

lib_filter=(sandbox::landlock_ns semantic::tests::a_partial_sandbox_is_admitted)
int_filter=(linux_sandbox:: cargo_env_loader_var::a_sandboxed_server_maps_no_project_dylib_and_still_resolves)
engine_tests=(semantic_runs_no_project_code indexing_writes_nothing_and_stays_offline conformance)
engine_args=""
for t in "${engine_tests[@]}"; do
	engine_args="$engine_args --test $t"
done

docker build -q -t "$tag" - >/dev/null <<EOF
FROM $image
RUN rustup component add rust-analyzer && chmod -R a+rwX "\$RUSTUP_HOME" "\$CARGO_HOME"
EOF

# in_container <user> <seccomp: unconfined|default> <bash command>
in_container() {
	local user="$1" seccomp="$2" command="$3"
	local security=()
	if [ "$seccomp" = unconfined ]; then
		security=(--security-opt seccomp=unconfined)
	fi
	# `${a[@]+...}`: bash 3.2 (macOS) treats an empty array as unset under `set -u`.
	docker run --rm ${security[@]+"${security[@]}"} -u "$user" \
		-e HOME=/tmp -e CARGO_TARGET_DIR=/target -e CARGO_TERM_COLOR=never \
		-v "$target_volume:/target" -v "$registry_volume:/usr/local/cargo/registry" \
		-v "$src:/src:ro" -w /src "$tag" bash -c "$command"
}

sandbox_tests="cargo test -p g-mesh-plugin-rust --locked --lib -- ${lib_filter[*]} --show-output \
	&& cargo test -p g-mesh-plugin-rust --locked --test semantic_runs_no_project_code -- ${int_filter[*]} --show-output"
all_tests="cargo test -p g-mesh-plugin-rust --locked --lib -- ${lib_filter[*]} --show-output \
	&& cargo test -p g-mesh-plugin-rust --locked$engine_args -- --show-output"
# A uid-1000 run must be able to write what a root build left.
open_up="chmod -R a+rwX /target /usr/local/cargo/registry"

log="$(mktemp)"
trap 'rm -f "$log"' EXIT

echo "build ($image)"
# `conformance` drives the plugin through the `g-mesh` binary's kit.
in_container 0 unconfined "cargo build -p g-mesh --bin g-mesh --locked \
	&& cargo test -p g-mesh-plugin-rust --locked --lib$engine_args --no-run && $open_up" \
	>"$log" 2>&1 || {
	cat "$log"
	echo "FAIL build"
	exit 1
}

pass() {
	local name="$1" user="$2" seccomp="$3" n="$4" tests="$5" i
	for i in $(seq 1 "$n"); do
		if ! in_container "$user" "$seccomp" "$tests; status=\$?; [ $user = 0 ] && $open_up; exit \$status" >"$log" 2>&1; then
			cat "$log"
			echo "FAIL $name run $i/$n"
			exit 1
		fi
		grep -E '^test result:' "$log" | sed "s/^/$name run $i\/$n: /"
		grep -E ': (skipped|runs as uid)' "$log" | sort -u | sed "s/^/$name run $i\/$n:   /" || true
	done
	echo "PASS $name x$n"
}

pass root 0 unconfined "$runs" "$all_tests"
pass uid1000 1000 unconfined "$runs" "$all_tests"
pass uid1000-default-seccomp 1000 default 1 "$sandbox_tests"
echo "PASS all"
