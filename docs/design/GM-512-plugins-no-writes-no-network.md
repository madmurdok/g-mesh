# GM-512: indexing writes nothing under the project root and reaches no network

Status: design (slice S1), awaiting owner decisions D1-D4.
Branch: `fix/GM-512-plugins-no-writes-no-network`.
Builds on GM-513 (`docs/design/GM-513-semantic-servers-no-project-code.md`,
branch `fix/GM-513-semantic-servers-run-no-project-code`), whose code lands
first. The state designed against is GM-513's result:

- rust-analyzer: `cargo.buildScripts.enable = false`, `procMacro.enable =
  false`, `RUSTUP_TOOLCHAIN` pinned to `rustup default`, `RUSTC`/`CARGO`/
  wrapper overrides in `SemanticConfig::env`.
- pyright: `python.pythonPath` = a sentinel, venv site-packages as
  `python.analysis.extraPaths`; python no longer takes pyright from
  `<root>/node_modules/.bin`.
- go: `cfg.Env` adds `GOTOOLCHAIN=local`, `GOPACKAGESDRIVER=off`; cgo stays on.

## Summary

| Plugin | Writes under the root (GM-513 state) | Reaches the network (GM-513 state) | Proposed prevention |
|---|---|---|---|
| rust | none observed (no `target/`, no `Cargo.lock`) | yes: `cargo metadata` fetches uncached registry and git deps (observed: `github.com`, `api.github.com`, `index.crates.io`) | `CARGO_NET_OFFLINE=true` in the GM-513 env map |
| python | none observed (pyright writes no cache) | only through the `npx --yes --package pyright` candidate: always contacts the registry, even with a warm cache, and the registry is chosen by the project's `.npmrc` | drop the `npx` candidate for python (D2) |
| go | `go.mod`/`go.sum` rewritten when the user's `GOFLAGS` (env or `go env -w`) holds `-mod=mod` (observed, no network needed) | yes: `go list -export -deps` downloads uncached modules from `GOPROXY` (observed `GET …/@v/v0.28.0.mod`, `.zip`) | `GOPROXY=off` in `cfg.Env`; `BuildFlags: -mod=readonly` (or `-mod=vendor` when vendored) |
| core | none: state is `~/.g-mesh/projects/<hash>` (`storage::connection::project_dir`) | none: the embedding model is fetched only by the user-typed `g-mesh model fetch` (`core/src/cli/model.rs` is the only `ureq` user; `core/src/embedding/model.rs` module doc) | nothing to change |

Every prevention costs only what the machine does not already have: a
dependency never fetched on this machine (cargo registry/git cache, Go module
cache) no longer resolves; local code still does (observed for both).

## How this was established

Scratch: `/private/tmp/claude-502/.../scratchpad/gm512` (`run.sh`,
`logger.py`, `summary.txt`; reused `lsp_drive.py` from GM-513 S1). Load
average 48-61 at the start, 7 at the end; timings indicative only.

- **Writes**: each arm copies its fixture, records `find <root> | sort`,
  touches a stamp, runs, then reports `find -newer stamp` and added paths.
- **Network**: `logger.py` is a fail-loud sink on `127.0.0.1:18512`: it logs
  the request line and `Host` of every connection (proxy `CONNECT`, proxy
  `GET`, registry `GET`) and answers 403. Arms point every proxy variable at
  it (`HTTP(S)_PROXY`, `CARGO_HTTP_PROXY`, `npm_config_(https_)proxy`), or
  `GOPROXY`/a registry URL directly. An attempt is a log line; no sniffer.
- rust-analyzer 1.97.1 via `rustup which --toolchain stable-x86_64-apple-darwin`
  (the GM-513 pin), `PROP` options (build scripts and proc macros off), the
  GM-513 wrapper env, `CARGO_NET_RETRY=0`. pyright 1.1.414 with the GM-513
  settings. Go 1.27.0, `goload` (the plugin's `packages.Config`), with
  `GOTOOLCHAIN=local GOPACKAGESDRIVER=off`.

### Rust (`rnet`: `app` with `itoa = "1"` + a git dep on `dtolnay/anyhow`, no `Cargo.lock`; `rwarm`: `itoa` only)

| Arm | CARGO_HOME | Env | Definition asked | Result | Network | Written under root |
|---|---|---|---|---|---|---|
| rn_cold_default | empty | - | local `util::helper` | resolves | 6 (`CONNECT github.com`, `api.github.com`) | nothing |
| rn_cold_offline | empty | `CARGO_NET_OFFLINE=true` | local | resolves | 0 | nothing |
| rn_cold_nodeps | empty | RA `cargo.noDeps = true` | local | resolves | 0 | nothing |
| rn_warm_offline_dep | user's | offline | `itoa::Buffer` | resolves into `~/.cargo/registry/src/…/itoa-1.0.18` | 0 | nothing |
| rn_warm_offline_local | user's | offline | local | resolves | 0 | nothing |
| rn_warm_online_dep | user's | - (network unreachable) | `itoa::Buffer` | **`[]`** | 3 (`CONNECT index.crates.io`) | nothing |

Read: with no `Cargo.lock`, `cargo metadata` re-resolves against the index
even when the crate is cached; when that fails, rust-analyzer loses every
dependency. Offline makes cargo resolve from the local cache, so on a machine
whose cache is warm it is *better* than online-but-unreachable. Local
definitions resolve in every arm (rust-analyzer falls back to a crate graph
without the failed deps). No arm wrote `target/` or `Cargo.lock`.

### Python (`pfx`: `main.py` imports `vpkg` from `.venv/lib/python3.9/site-packages`)

| Arm | Run | Result | Network | Written under root |
|---|---|---|---|---|
| p_513 | pyright-langserver, `pythonPath=/usr/bin/false` + `extraPaths` | `vfunc` resolves | 0 | nothing |
| npx_cold_proxy | `npx --yes --package pyright pyright --version`, empty npm cache, `cwd` = root | 403 | 2 (`registry.npmjs.org`) | nothing |
| npx_warm_proxy | same, cache holding pyright 1.1.414 | 403 | 1 (`registry.npmjs.org`) | nothing |
| npx_warm_proxy_preferoffline | + `npm_config_prefer_offline=true` | `pyright 1.1.414` | 0 | nothing |
| npx_warm_offline | + `npm_config_offline=true` | `pyright 1.1.414` | 0 | nothing |
| npx_cold_project_npmrc | project `.npmrc` `registry=http://127.0.0.1:18512/evil-registry/`, no proxy | 403 | 2 (`GET /evil-registry/pyright`, `/evil-registry/npm`) | nothing |
| npx_cold_project_npmrc_envregistry | same + env `npm_config_registry=https://registry.npmjs.org/` | 403 | 2 (`registry.npmjs.org`): env beats the project file | nothing |

Read: `npx` reaches the registry on every start unless told otherwise, and
since both its probe (`lsp::resolve::probe`, inherited `cwd`) and the server
(`LspClient::start`, `cwd = root`) run inside the project, the project's
`.npmrc` picks the registry - i.e. which `pyright` package gets downloaded
and **executed**. That is a GM-513-class hole (project-chosen code runs)
riding on a GM-512 one.

### Go (`gnet`: `require golang.org/x/text v0.28.0` with `go.sum`; `gnosum`: same without `go.sum`)

| Arm | GOMODCACHE | Env | Result | Network | Written under root |
|---|---|---|---|---|---|
| g_cold_proxy | empty | `GOPROXY=<sink>` | `Seven` resolves; `x/text` import error | 2 (`GET /golang.org/x/text/@v/v0.28.0.mod`, `.zip`) | nothing |
| g_cold_off | empty | `GOPROXY=off` | same | 0 | nothing |
| g_warm_off | user's | `GOPROXY=off` | everything resolves, 0 errors | 0 | nothing |
| g_nosum_default | empty | sink | import error (missing `go.sum` entry) | 0 | nothing |
| g_nosum_modmod | empty | sink, `GOFLAGS=-mod=mod` | import error | 2 (`.mod`, `.zip`) | nothing (download failed) |
| g_nosum_modmod_warm_off | user's | `GOPROXY=off`, `GOFLAGS=-mod=mod` | resolves | 0 | **`go.sum` created** |
| gl_nosum_modmod_flag_readonly | user's | `GOFLAGS=-mod=mod`, `go list -mod=readonly` | "missing go.sum entry" | 0 | nothing: the flag beats `GOFLAGS` |
| gl_nosum_modmod_noflag | user's | `GOFLAGS=-mod=mod`, `go list` | resolves | 0 | **`go.sum` created** |

Two further checks (no network, `GOPROXY=off`, `GOFLAGS=-mod=mod`): a
`go.mod` without a `go` line gets `go 1.27.0` written into it; a
`replace`d-but-not-required dependency gets a `require` line added. With
`-mod=readonly` on the command line neither file changed. In a vendored
module (`vendor/modules.txt`), `-mod=readonly` makes `go list` ignore
`vendor/` and try to download (`go: downloading golang.org/x/text v0.28.0`,
then "module lookup disabled by GOPROXY=off"), so the flag must be
`-mod=vendor` there.

`GOCACHE` (`~/Library/Caches/go-build`) and `GOMODCACHE` (`~/go/pkg/mod`)
are written by `go list -export`; both come from the user's environment,
never from the module, and are outside the root unless the user put them
there.

## 1. Inventory per plugin

### rust

Writes under root:
1. `target/` from `cargo check --compile-time-deps` - gone with GM-513 (build
   scripts and proc macros off; `checkOnSave` was already off). Re-enabling
   either brings it back (GM-513 r_base: 2.6 MB).
2. `Cargo.lock` from `cargo metadata` - rust-analyzer 1.97.1 passes
   `-Zlockfile-path` (GM-513 evil-cargo log), so none is written. This is a
   property of the server version, not of our config: an older
   rust-analyzer (a non-rustup install on `PATH`) may write it. The test pins
   it for the version CI installs; no prevention exists short of `--locked`,
   which fails every lock-less project (rejected).
3. The structural tier and the plugin process write nothing (no process
   spawns other than the server and `rustup which`; read the
   `Command::new` sites).

Network:
1. `cargo metadata` resolving dependencies not in `$CARGO_HOME`'s registry/git
   cache, and **any** registry dependency when there is no `Cargo.lock` (index
   refresh). Trigger: opening a project whose dependencies this machine never
   fetched, or one without a lockfile. Observed.
2. rustup toolchain installs (`rust-toolchain.toml channel`) - gone with
   GM-513's pin.
3. rust-analyzer itself fetches nothing.

### python

Writes: none. pyright keeps no on-disk cache; GM-513 removed the interpreter
(whose `-c` scripts could have written `__pycache__` under the root).

Network:
1. The `npx --yes --package pyright` candidate (`lsp::npm_candidates`, last
   origin): runs when `pyright-langserver` is not on `PATH` (and, after
   GM-513, no longer from `node_modules/.bin`). Contacts the registry even
   with a warm cache; the registry is the project's `.npmrc` choice.
2. pyright itself: none.

### go

Writes under root:
1. `go.mod`/`go.sum` when the effective `-mod` is `mod` - set by the user's
   `GOFLAGS` (env or `go env -w`). The default (`readonly`, or `vendor` when
   vendored) never writes. Observed.
2. Nothing else: `-export` output goes to `GOCACHE`.

Network:
1. `go list -deps -export` downloads every required module missing from
   `GOMODCACHE` (`GOPROXY` default `https://proxy.golang.org,direct`;
   `direct` means a VCS fetch from the module's host). Observed.
2. `sum.golang.org` lookups: only when go.sum entries are being added, which
   `-mod=readonly` forbids and `GOPROXY=off` makes moot.
3. Toolchain downloads - gone with GM-513's `GOTOOLCHAIN=local`.

## 2. Prevention and its cost

### rust: `CARGO_NET_OFFLINE=true`

Added to the env map GM-513 builds in `semantic::engine` (GM-513 §7 asks for
exactly this: one mechanism). Env rather than `cargo.extraArgs: ["--offline"]`:
it reaches every cargo invocation rust-analyzer makes (`metadata`,
`locate-project`, `config get`, `rustc --print cfg`), and a project
`.cargo/config.toml` `net.offline = false` does not override an env var
(cargo: env beats config files).

Cost: dependencies absent from this machine's cargo cache do not resolve
(edges into them are lost; local edges remain - rn_cold_offline). With no
`Cargo.lock` but a warm cache, offline is strictly better than today
whenever the network is down (rn_warm_online_dep `[]` vs
rn_warm_offline_dep resolves). A user who wants deps resolved runs `cargo
fetch` once; the log line on a failed metadata should say so.

Rejected: `cargo.noDeps = true` (0 network, 3x faster, but drops cached deps
too).

### python: drop the `npx` candidate (D2)

`npm_candidates` grows an origin selector; python asks for `PATH` only (GM-513
already removes `node_modules/.bin`), TypeScript keeps today's list until its
own task. The remedy message names `npm install -g pyright`.

Cost: a machine with no global pyright loses the python semantic tier
(structural stays) with a one-line log naming the remedy. Today such a
machine downloads pyright on every daemon start.

Alternative (D2 b): keep `npx` with `npm_config_offline=true` and
`npm_config_registry=https://registry.npmjs.org/` in the probe's and
server's env: no network (npx_warm_offline), the project's `.npmrc` registry
overridden (npx_cold_project_npmrc_envregistry), works only for a user whose
npm cache already holds pyright.

### go: `GOPROXY=off` + `-mod=readonly`/`-mod=vendor`

In `semanticEngine.load` (`plugins/go/semantic.go:311`):

```go
cfg.Env = append(os.Environ(), "GOTOOLCHAIN=local", "GOPACKAGESDRIVER=off", // GM-513
	"GOPROXY=off")                                                          // GM-512
cfg.BuildFlags = []string{modFlag(dir)} // "-mod=vendor" if the governing
                                        // vendor/modules.txt exists, else "-mod=readonly"
```

`BuildFlags`, not `GOFLAGS`: an env `GOFLAGS` would replace the user's other
flags; a command-line `-mod` overrides only `-mod` (observed). `os/exec`
keeps the last of duplicate env keys, so the appended values win.
`modFlag` looks for `vendor/modules.txt` beside the governing `go.work` when
there is one, else beside `go.mod`; in workspace mode only `readonly` and
`vendor` are legal, which is all this emits.

Cost: modules missing from `GOMODCACHE` do not type-check, so references into
them are lost and the importing package carries an error (local code still
resolves - g_cold_off `Seven=true`). A module whose `go.sum` lacks entries
loses those imports (already true by default today). Users with
`GOFLAGS=-mod=mod` stop getting their `go.mod` tidied by g-mesh, which was
never wanted.

Not set: `GOSUMDB=off` (would weaken verification if anything ever did
download), `GOFLAGS` (see above), `GOMODCACHE`/`GOCACHE` (user's, outside
the root).

## 3. Tests

Shape mirrors the deleted `security.test.ts` (339: no writes; 420: no
network): a fixture copied into a temp dir, a **full index plus semantic
pass** driven in-process the way the shipped plugin does it, then three
assertions: (1) sanity - a semantic edge the fixture is built to need exists,
so the server really ran with the hostile setup in force; (2) the root's
snapshot is unchanged; (3) the network sink saw no connection.

Snapshot = every path under the root with its type, length and content
hash (mtime alone is too coarse; content alone misses new empty dirs). Sink =
a `127.0.0.1:0` TCP listener in the test that counts accepts (Rust: a
`std::net::TcpListener` thread; Go: `net.Listen` goroutine). Proxy env points
at it, so every attempt is a counted connection and fails fast.

| Plugin | Test | Fixture | Pass driven by | Sanity | Controls (revert the code, test must fail) |
|---|---|---|---|---|---|
| rust | `plugins/rust/tests/indexing_writes_nothing_and_stays_offline.rs` (own binary, one `#[test]`, sets process env first: `CARGO_HOME`=empty temp, `CARGO_HTTP_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY`=sink, `CARGO_NET_RETRY=0`) | two, run in sequence: **A** network - `itoa = "1"` + a git dep, no `Cargo.lock`; **B** writes - local workspace with a `build.rs`, no deps, no `Cargo.lock` | walk + `RustExtractor` into an `SdkIndex` (as `semantic_pass_measurement::index_corpus`), then `semantic::engine(root)?.answer(&[], &index)` - through `engine`, so the env and pin are under test | A and B: a cross-file `CALLS` edge to the local function is `SourceTier::Semantic` | C1: drop `CARGO_NET_OFFLINE` → sink counts `CONNECT` (rn_cold_default: 6). C2: set `cargo.buildScripts.enable = true` in `plugin.toml` → `target/` appears in B (GM-513 r_base). `Cargo.lock` absence: pinned, no control (no code of ours prevents it; see 1.rust.2) |
| python | `plugins/python/tests/indexing_writes_nothing_and_stays_offline.rs` (sink as proxy env) + unit test in `semantic.rs` that `candidates()` has no `npx` origin | `pfx` (venv site-packages dir, `main.py`) plus a project `.npmrc` whose `registry` is the sink | walk + extractor, then `semantic::engine(root)?.answer(&[], &index)` | `vfunc` resolves into the venv | C3: restore the `npx` origin in python's `candidates` → unit test fails (naming the origin). The integration part has no revertable fix of ours (pyright writes nothing, opens nothing): it is a pin, as stated in the test's doc |
| go | `plugins/go/semantic_offline_test.go` | **A** network - `require example.com/gm512missing v1.0.0` + a `go.sum` with syntactically valid placeholder `h1:` lines (the GET happens before verification; the test asserts the attempt, not a hash), `t.Setenv("GOPROXY", sink URL)`, `GOMODCACHE`=`t.TempDir()`; **B** writes - `t.Setenv("GOFLAGS", "-mod=mod")`, module with no `go` line and a `replace`d-not-required local dep; **B'** vendored variant (resolves from `vendor/`, no write) | `runBulkIndex` then `newPluginState(root).handleSemanticPass(nil)` | A and B: a local caller edge onto `Seven` (B's import of the replaced dep is a load error under readonly, by design); B': the call into the vendored dep resolves | C4: drop `GOPROXY=off` → sink counts `GET …/@v/v1.0.0.mod`. C5: drop the `-mod` build flag → `go.mod` rewritten in B. C6: always emit `-mod=readonly` (no vendor case) → B' loses the dep edge |

Notes:
- Edition 2021 in both Rust plugins, so `std::env::set_var` is safe to call;
  it must still run before the engine spawns anything, hence one `#[test]`
  per binary.
- `GOMODCACHE` in a `t.TempDir()`: nothing is downloaded with the fix in
  place; under C4 a failed download leaves no read-only files either (it
  403s before extraction).
- CI: rust-analyzer is installed on all four runners and pyright on
  linux/macOS (`.github/workflows/ci.yml:481-581`); Go 1.27 job at `:628`.
  The tests need no network themselves (they assert none), so they run as
  part of the normal jobs. Where pyright is absent the python integration
  test fails naming the missing binary, as the existing python conformance
  tests do; the Windows job follows whatever GM-513's python test does there.
- These drive real servers and processes: 5x repeats in the tests slice and
  in verify, per the standing rule.

## 4. Edit map

Line numbers are today's (`90a2d87`); GM-513 lands first and edits the same
functions, so the implement slice re-reads them by symbol.

Change:
- `plugins/rust/src/semantic.rs` - `engine` (`:80-101`): add
  `CARGO_NET_OFFLINE=true` to the env map GM-513 introduces here (overlap:
  same function, same map; add one entry, no second mechanism). Extend the
  manifest test `the_shipped_manifest_configures_the_server_this_module_expects`
  (`:218-252`) only if GM-513 moved the env into the manifest.
- `plugins/rust/plugin.toml:129-160` - the comment block GM-513 rewrites:
  add one line that the tier is offline and why (link the ADR).
- `plugins/sdk/src/lsp/resolve.rs` - `npm_candidates` (`:126-161`): GM-513
  adds a way to omit the project-local origin; make it one origin selector
  (e.g. `NpmOrigins { project_local: bool, npx: bool }`) so GM-512 only flips
  `npx` for python. Tests `a_bare_name_is_path_then_node_modules_then_npx`
  (`:395`), `the_npx_candidate_names_the_package_and_probes_the_probe_bin`
  (`:446`) keep TS's full list.
- `plugins/python/src/semantic.rs` - `candidates` (`:335-337`), `resolve`'s
  remedy string (`:323-333`), module doc decisions on discovery (`:38-60`,
  GM-513 also edits it). Tests near `a_project_local_install_is_found_and_probed_for_real`
  (`:536`).
- `plugins/python/plugin.toml:106-114` - the "then `npx`" comment.
- `plugins/go/semantic.go` - `load` (`:311-330`): `GOPROXY=off` into GM-513's
  `cfg.Env` line, `cfg.BuildFlags`; new `modFlag(dir string) string` near
  `moduleDirFor` (`:661`). Doc at `:37-55` (GM-513 edits it too).
- New tests as in section 3.
- ADR: extend GM-513's new ADR ("semantic tiers execute no project code")
  with "and reach no network, write nothing under the root", or a sibling
  ADR; the code comments state the invariant and link it.

Read for context: `plugins/rust/tests/semantic_pass_measurement.rs:54-80`
(`index_corpus`, the in-process index to copy), `plugins/sdk/src/lsp/config.rs`
(`SemanticConfig::env` merge), `plugins/sdk/src/lsp/client.rs:222-240`
(`LspClient::start`, `cwd = root`), `plugins/go/control.go:59, 269`
(`newPluginState`, `handleSemanticPass`), `plugins/go/bulkindex.go:45`
(`runBulkIndex`), `plugins/go/semantic_test.go:106-136` (fixture helpers).

g-mesh calls relied on (project `g-mesh-wt-gm512`):
- `find_references npm_candidates` → `plugins/python/src/semantic.rs::candidates`,
  `plugins/typescript/src/semantic.rs::candidates`, re-export in
  `plugins/sdk/src/lsp/mod.rs` (so an unflagged change hits TS).
- `find_callers lsp::resolve::probe` → only `lsp::resolve::resolve` in
  production (plus tests): the probe inherits the plugin's `cwd`.
- `find_callers semanticEngine.load` → only `semanticEngine.loadFor`.
- `get_file_outline plugins/sdk/src/testing.rs` → `PluginCheck::run` drives
  `g-mesh plugins check`, which edits its fixture copy itself
  (`files_created`, version bump), so it cannot host a "nothing changed"
  assertion; hence the in-process drive above.
- `Command::new`/`exec.Command` sites: grep (non-code-graph question).

## 5. Owner decisions

**D1. Rust dependencies this machine never fetched.**
Today: rust-analyzer's `cargo metadata` downloads registry and git
dependencies missing from `~/.cargo` (and refreshes the crates.io index for
any project without a `Cargo.lock`) every time a project is opened. Change:
`CARGO_NET_OFFLINE=true`. Example: a freshly cloned project whose deps were
never built here - today `itoa::Buffer` resolves after a download (or, with
no network, does not resolve at all); after, it resolves only if `itoa` is
already in `~/.cargo`, while every call inside the project still resolves.
- (a) **Offline always (Recommended).** Benefit: no network from the rust
  tier, and better than today when the network is down (cached deps resolve
  offline, not online). Risk: deps never fetched on this machine lose their
  edges until the user runs `cargo fetch`.
- (b) Offline by default, opt-in to network in the *user's* g-mesh config.
  Benefit: full dep edges for users who want them. Risk: a config knob and a
  second tested path.
- (c) Leave online, document the exception (trigger: uncached deps or no
  `Cargo.lock`). Benefit: no edge loss with a working network. Risk: the
  task's criterion is met only by documentation.

**D2. Python's `npx` fallback for pyright.**
Today: when `pyright-langserver` is not on `PATH`, the plugin runs
`npx --yes --package pyright` inside the project: it contacts the registry on
every start, and the project's `.npmrc` chooses that registry, i.e. which
package is downloaded and executed. Change: python stops using `npx`.
Example: a user who never ran `npm install -g pyright` today gets pyright
downloaded on first use; after, the log says "pyright not found, install it
with `npm install -g pyright`" and Python stays structural-only.
- (a) **Drop `npx` for python; TypeScript in a follow-up task
  (Recommended).** Benefit: no network, no project-chosen package. Risk:
  users without a global pyright lose the semantic tier until they install
  it.
- (b) Keep `npx` with `npm_config_offline=true` and the registry pinned in
  env. Benefit: works for users whose npm cache already holds pyright, never
  touches the network, ignores the project's registry. Risk: works for few
  users, more env to test; the project's `.npmrc` still applies to other
  keys.
- (c) Keep today's `npx`, document. Risk: network on every start and a
  project-chosen registry runs code (also a GM-513 violation).

**D3. Go modules missing from the module cache.**
Today: `go list` downloads every required module not in `~/go/pkg/mod`
from `proxy.golang.org` (or the module's VCS host). Change: `GOPROXY=off`.
Example: a cloned module importing `golang.org/x/text` that this machine
never downloaded - today it downloads and resolves; after, the import is an
error in that package, `language.English` gets no edge, and the package's
own functions still resolve.
- (a) **`GOPROXY=off` always (Recommended).** Benefit: no network from the
  go tier; one env var. Risk: edges into uncached modules are lost until the
  user runs `go mod download`.
- (b) Off by default, opt-in in the user's g-mesh config. Benefit/risk as
  D1 (b).
- (c) Leave online, document the trigger. Risk: criterion met by
  documentation only.

**D4. Go's `-mod` mode when the user's `GOFLAGS` says `-mod=mod`.**
Today: a user who ran `go env -w GOFLAGS=-mod=mod` gets their projects'
`go.mod`/`go.sum` rewritten by g-mesh's indexing (observed: a `go` line
added, a `require` added, a `go.sum` created). Change: pass `-mod=readonly`
on `go list`'s command line, or `-mod=vendor` when the module (or workspace)
is vendored. Example: that user's project keeps its `go.mod` byte-for-byte;
an import whose requirement is missing becomes a load error instead of a
silent `go.mod` edit.
- (a) **Explicit `-mod=readonly`/`-mod=vendor` build flag (Recommended).**
  Benefit: indexing never writes `go.mod`/`go.sum`; the user's other `GOFLAGS`
  stay. Risk: a small vendor-detection function to get right (`go.work`
  layout included), pinned by C6.
- (b) Override the whole `GOFLAGS` env with `-mod=readonly`. Benefit: one
  line. Risk: drops the user's other flags (`-tags`, `-buildvcs`, …) and
  breaks vendored modules.
- (c) Leave it; document that `-mod=mod` users get writes. Risk: the "writes
  nothing under the root" criterion fails for them.

Not a decision: `Cargo.lock` absence depends on rust-analyzer passing
`-Zlockfile-path` (1.97.1 does); it is pinned by the rust test for CI's
version and documented in the ADR as a server-version property.

Out of scope, worth a task: the TypeScript plugin has the same `npx` and
`node_modules/.bin` origins (shared `npm_candidates`) and no test of either
property since 874cc75.
