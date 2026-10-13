# 0033. Semantic tiers reach no network and write nothing under the project root

## Status
Accepted 2026-10-13 (GM-512, owner decisions D1-D4). Experiments, the
per-plugin inventory and the options:
[`GM-512-plugins-no-writes-no-network.md`](../design/GM-512-plugins-no-writes-no-network.md).
Builds on [ADR 0032](0032-semantic-tiers-execute-no-project-code.md).

## Context
With ADR 0032 in place no semantic tier runs project code, but two of them
still acted on the network or the project tree when a project was opened:

- **rust-analyzer**'s `cargo metadata` fetched registry and git dependencies
  missing from the local cargo cache, and refreshed the crates.io index for
  any project without a `Cargo.lock`.
- **go/packages**' `go list -deps -export` downloaded every required module
  missing from `GOMODCACHE` from `GOPROXY` (`direct` meaning the module's
  own host), and rewrote `go.mod`/`go.sum` when the user's `GOFLAGS` held
  `-mod=mod`.
- **pyright** writes nothing and opens no connection. Its one network path
  was the `npx` candidate, which ADR 0032 D4 already removed.
- **core** keeps its state under `~/.g-mesh/projects/<hash>` and reaches the
  network only for the user-typed `g-mesh model fetch`.

Indexing a project must leave the project tree byte-for-byte as it was and
must not contact any host the project's files can choose.

## Decision
A semantic tier works from what is already on this machine and writes
nothing under the project root.

- **D1 - rust: `CARGO_NET_OFFLINE=true`, always.** Set in the server's
  environment next to ADR 0032's toolchain pin
  (`plugins/rust/src/semantic.rs`, `pinned_env`), so it reaches every cargo
  rust-analyzer starts. Environment outranks a project's
  `.cargo/config.toml` `net.offline`.
- **D2 - python: no `npx`.** Done by ADR 0032 D4; pyright comes from `PATH`
  only. Nothing further.
- **D3 - go: `GOPROXY=off`, `GONOPROXY=-` and `GOSUMDB=off`, always.** Appended to
  `go list`'s environment (`plugins/go/semantic.go`, `loadEnv`); later
  entries win over the user's. `GOPROXY=off` alone covers only modules go
  fetches through the proxy list: a module matched by `GONOPROXY` (which
  defaults to `GOPRIVATE`, from the environment or `go env -w`) is fetched
  directly from its host, `https://<path>?go-get=1` then a VCS clone, and
  `GOVCS=*:off` does not stop the HTTPS step. `GONOPROXY=-` is a pattern no
  module path matches (a path never begins with `-`), so every module goes
  through `off`. `GOSUMDB=off`: in the exception below go looks up the
  checksums it records in the user's `GOSUMDB` directly, not through
  `GOPROXY`, sending module paths there (measured, go1.27: `CONNECT
  sum.golang.org:443`; a private path too if `GOPRIVATE` were cleared). With
  `GOPROXY=off` nothing is downloaded. The cost: in the exception below, the
  hashes go records in `go.work.sum` come from the module cache without a
  checksum-database check (the cache was checked when it was filled); the
  owner accepted this over sending module paths out. `GOINSECURE` and `GONOSUMDB` re-enable no fetch
  with `GOPROXY=off` (measured), so they stay the user's.
- **D4 - go: an explicit `-mod` build flag.** `-mod=vendor` when the
  governing `go.work` (else `go.mod`) has `vendor/modules.txt` beside it,
  `-mod=readonly` otherwise, none outside module mode (`modFlag`). The
  governing files are the ones `go env GOWORK GOMOD` resolves from the load's
  directory under the load's environment, so the go env file (`go env -w`)
  counts as well as the process environment. A
  command-line `-mod` overrides only the `-mod` in the user's `GOFLAGS`; the
  user's other flags stay. `GOFLAGS` itself is not replaced.

Exception, documented rather than prevented (owner decision): **go itself
writes `go.work.sum` in workspace mode.** Trigger: the governing `go.work`
has no `vendor/modules.txt` beside it (so the flag is `-mod=readonly`), and
a member module's `go.sum` lacks the entries for a required module that is
in `GOMODCACHE`. go then records those checksums in `go.work.sum` beside
`go.work`, under the project root; `-mod=readonly` does not apply its go.sum
check in workspace mode. Module mode (`GOWORK=off`, or no `go.work`) writes nothing, and a
member whose `go.sum` is complete writes nothing. A test pins the exception
so a go release that changes it shows up.

Not set: `GOMODCACHE`/`GOCACHE`/`CARGO_HOME` (the user's, outside the root).

Rejected: rust-analyzer `cargo.noDeps = true` (no network and faster, but
drops cached dependencies too); a user-config opt-in to network (a knob and a
second tested path); `--locked` against `Cargo.lock` creation (fails every
project without a lockfile); keeping python's `npx` with an offline npm
config (works only for a user whose npm cache already holds pyright).

## Consequences
- **Dependencies never fetched on this machine do not resolve:** a cargo
  dependency missing from `~/.cargo`, or a Go module missing from
  `GOMODCACHE`, loses its edges, and a Go package importing it carries a load
  error. Local code still resolves. The remedy is the user's own `cargo
  fetch` / `go mod download`; the rust tier's start-up log line says so.
- With the network down, cached dependencies now resolve where before the
  online attempt failed the whole `cargo metadata`.
- A Go user with `GOFLAGS=-mod=mod` no longer has `go.mod` tidied by g-mesh.
- A Go module matched by the user's `GOPRIVATE`/`GONOPROXY` and missing from
  `GOMODCACHE` is not fetched either; it loses its edges like any other.
- A Go workspace whose members' `go.sum` files are incomplete can gain a
  `go.work.sum` (the exception above).
- `Cargo.lock` is not written because rust-analyzer 1.97.1 passes
  `-Zlockfile-path`; an older rust-analyzer may write one. No configuration
  of ours prevents that; the tests pin it for the version CI installs.
- Follow-up: TypeScript's `npx` candidate (same network exposure as
  python's had), in its own task.
