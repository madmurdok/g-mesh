# GM-572: can `.cargo/config.toml` rustflags / `[env]` / RUSTC_BOOTSTRAP reach the plugin's rustc/cargo invocations and execute project code?

Status: probe + design (slice S1), awaiting owner decisions D1-D2.
Branch: `docs/GM-572-rustflags-rustc-bootstrap-probe`.

## Question

ADR 0032 (GM-513) pinned the Rust semantic tier so build scripts, proc
macros, `build.rustc`, the `rustc` wrappers and a `rust-toolchain.toml`
toolchain cannot run. rust-analyzer still runs `cargo metadata` and
`rustc --print …` probes, and runs some of them with `RUSTC_BOOTSTRAP=1`.
This slice asks whether a cloned project's `.cargo/config.toml` can still
reach those invocations through a channel GM-513 did not pin -
`[build]`/`[target.*] rustflags`, the `[env]` table,
`-Zcodegen-backend=<project dylib>`, `-Zcrate-attr` - and execute code.

The test drives rust-analyzer with the **plugin's own configuration**, not
rust-analyzer defaults: the manifest's init options
(`checkOnSave=false`, `cargo.buildScripts.enable=false`,
`procMacro.enable=false`; `plugins/rust/plugin.toml`) and the exact
environment `pinned_env` builds (`plugins/rust/src/semantic.rs:171`):
`CARGO_NET_OFFLINE=true`, `RUSTUP_TOOLCHAIN`, `RUSTC`/`CARGO_BUILD_RUSTC`/
`CARGO` = the toolchain's own binaries, and the four `rustc` wrapper
variables emptied.

## Method

Reused GM-513's harness (`scratchpad/gm513/lsp_drive.py`, a minimal LSP
client that waits for rust-analyzer's `quiescent` status and asks one
`definition`), driven with `cwd` = a hostile fixture, server =
`rustup which rust-analyzer` (rust-analyzer 1.97.1, cargo 1.97.1), toolchain
`stable-x86_64-apple-darwin`. Scratch:
`scratchpad/gm572` (`run.sh`, `run2.sh`, `cfx/`, `cfx_force/`, `shims/`,
`logs/`). `uptime` at start: load average 41-61 (8-day-up host); timings
indicative only.

Fixture `cfx` is a one-crate workspace whose `.cargo/config.toml` arms every
channel at once, each with a distinct marker:

- `[build] rustc-wrapper = <shim>` -> writes `MARK_wrapper`.
- `[build] rustflags = ["--cfg","MARKFLAG_BUILD"]` and
  `[target.x86_64-apple-darwin] rustflags = ["--cfg","MARKFLAG_TARGET"]`
  -> the sentinel `--cfg` appears in the rustc argv log if the flags reach a
  rustc invocation (harmless cfgs chosen so the probe does not error; a real
  attacker would put `-Zcodegen-backend`/`-Zcrate-attr` here).
- `[env] DYLD_INSERT_LIBRARIES = <evil.dylib>` -> the dylib's constructor
  writes `MARK_DYLD` when dyld loads it into any non-SIP child.
- `[env] G572_ENV_MARK = "fired"` -> a child that sees it writes
  `MARK_ENV_rustc` (shows the `[env]` table reaches rustc's environment).

`RUSTC`/`CARGO_BUILD_RUSTC` were pointed at a logging shim that records each
rustc invocation's argv and its `RUSTC_BOOTSTRAP`/`RUSTFLAGS`/
`DYLD_INSERT_LIBRARIES`/`G572_ENV_MARK`, then `exec`s the real toolchain
rustc - transparent instrumentation, behaviour unchanged.

Sanity arms (protection absent) prove each hostile channel fires: plain
`cargo check` in the fixture, and `rustc --print cfg -Zcodegen-backend=<dylib>`
with `RUSTC_BOOTSTRAP=1`.

## Per-channel results

`[run]` = observed in an arm; `[read]` = read from source/config.

| Channel | Arm | Marker fired | Evidence |
|---|---|---|---|
| `[build] rustc-wrapper` / `rustc-workspace-wrapper` | protected (A) | **no** | `MARK_wrapper` absent; `pinned_env` empties the four wrapper vars `[read run]` |
| `[build] rustc` | (GM-513 f_all_env) | **no** | overridden by `RUSTC`/`CARGO_BUILD_RUSTC` `[read]` |
| `[build] rustflags` | protected (A) | **no** | `MARKFLAG_BUILD` 0 lines in rustc argv `[run]` |
| `[target.*] rustflags` | protected (A) | **yes** | `MARKFLAG_TARGET` on 15 rustc invocations; cargo applies target rustflags to its `--print` probes `[run]` |
| `RUSTC_BOOTSTRAP=1` on those probes | protected (A) | n/a | 9 of 39 rustc invocations ran with `RUSTC_BOOTSTRAP=1`; some carry both the bootstrap flag **and** `MARKFLAG_TARGET` (pids 76861/76991/77009) `[run]` |
| `-Zcodegen-backend=<project dylib>` via `[target.*] rustflags` | protected (A) + sanity T1 | **yes** | `rustc --print cfg -Zcodegen-backend=<dylib>` with `RUSTC_BOOTSTRAP=1` dlopens the backend and runs its constructor (`MARK_BACKEND`) before failing the symbol lookup - exactly the argv+env rust-analyzer produces in arm A `[run]` |
| `-Zcrate-attr=…` via rustflags | protected (A) | **no** | only compiling invocations apply crate attributes; all 27 flag-bearing invocations were `--print`, none compiled (buildScripts off) `[run]` |
| `[env]` arbitrary var -> rustc env | protected (A) | **yes** | `MARK_ENV_rustc` present (benign on its own) `[run]` |
| `[env] DYLD_INSERT_LIBRARIES` (dynamic-linker injection) | protected (A) | **yes** | `MARK_DYLD` present: cargo applies `[env]` to the rustc children of `cargo metadata`; the dylib's constructor executes `[run]` |

Sanity (protection absent), channels confirmed live: plain `cargo check` ->
`MARK_wrapper`, target rustflags reach rustc, `[env]` reaches rustc;
`RUSTFLAGS=-Zcodegen-backend=<dylib>` + `RUSTC_BOOTSTRAP=1` -> `MARK_BACKEND`;
`DYLD_INSERT_LIBRARIES=<dylib>` into the real rustc -> `MARK_DYLD`. No arm
created `target/` or `Cargo.lock` (ADR 0032/0033 hold for writes).

## Answer

**Yes.** Under the plugin's exact hardened configuration a cloned project's
`.cargo/config.toml` reaches the rustc/cargo invocations and executes code
through **two** channels GM-513 did not pin:

1. **`[env]` dynamic-linker injection.** `[env] DYLD_INSERT_LIBRARIES =
   <path>` (and the `LD_PRELOAD`/`LD_*` family on Linux) is applied by cargo
   to the rustc children it spawns for `cargo metadata`; dyld runs the named
   dylib's constructor. A repo can ship a prebuilt `.dylib` and point
   `[env]` at it - no build, no proc macro, no network. Observed: `MARK_DYLD`
   in arm A.
2. **`[target.*] rustflags` + `-Zcodegen-backend`.** Cargo applies the
   project's target rustflags to the `rustc --print` probes rust-analyzer
   runs, and rust-analyzer runs some of those probes with
   `RUSTC_BOOTSTRAP=1`, which makes stable rustc accept the `-Z` flag.
   `rustc --print … -Zcodegen-backend=<project dylib>` dlopens the dylib
   (its constructor runs) before erroring. Observed: `MARKFLAG_TARGET` on
   bootstrap-enabled invocations in arm A; the dlopen-executes step in
   sanity T1.

`[build] rustflags` alone did not reach rustc (cargo used target rustflags
when a `--target` was present); `-Zcrate-attr` did not execute (nothing
compiled). The GM-513 channels (wrappers, `build.rustc`) stay closed.

## Fix verification already run

- **Channel 2 closes with one env var.** Arm C re-drove rust-analyzer with
  `CARGO_ENCODED_RUSTFLAGS=""` added to `pinned_env`: `MARKFLAG_TARGET`
  dropped to **0** - cargo's rustflags precedence puts
  `CARGO_ENCODED_RUSTFLAGS` above both `[build]` and `[target.*]` config
  rustflags, so no project rustflags reach any rustc, and `-Zcodegen-backend`/
  `-Zcrate-attr` cannot be injected. `[run]`
- **Channel 1 does NOT close by env precedence.** Arm C also set
  `DYLD_INSERT_LIBRARIES=""` (and the `DYLD_*`/`LD_*` family) in the
  environment, yet `MARK_DYLD` still fired against the non-`force` config;
  arm D confirmed a `force = true` `[env]` entry fires too. Cargo applies the
  `[env]` value over an empty parent variable, so there is no
  environment-precedence fix for `[env]` in cargo 1.97, and no cargo switch
  disables the `[env]` table. A `rustc` wrapper cannot scrub it either:
  cargo applies `[env]` to the wrapper's own process, so on macOS the dylib
  injects into the wrapper before it can run. `[run]`

## Owner decisions

### D1 - close the rustflags / `-Zcodegen-backend` channel (recommended, in-scope)

- **Today:** `pinned_env` sets `RUSTC`, `CARGO`, `CARGO_NET_OFFLINE` and
  empties the wrapper vars, but sets nothing about rustflags, so cargo reads
  `[build]`/`[target.*] rustflags` from the project and feeds them to its
  rustc probes; with `RUSTC_BOOTSTRAP=1` those can carry `-Z` codegen flags.
- **Change:** add `CARGO_ENCODED_RUSTFLAGS=""` to `pinned_env` (environment
  outranks config, the same lever ADR 0032/0033 already use).
- **Example:** a repo with
  `[target.x86_64-apple-darwin] rustflags = ["-Zcodegen-backend=./evil.dylib"]`
  no longer has its rustflags seen by any rustc rust-analyzer runs.
- **Consequence:** rust-analyzer's `rustc` probes see no project rustflags;
  cfgs that only a project `--cfg` in rustflags would set are not resolved
  (consistent with ADR 0032's "project-chosen cfgs are not resolved").

  - **Option A (recommended): set `CARGO_ENCODED_RUSTFLAGS=""`.** Benefit:
    one line, one env var, closes `[build]` and `[target.*]` rustflags and
    therefore `-Zcodegen-backend` and `-Zcrate-attr` at once; verified (arm
    C). Risk: if a future feature wants a project's rustflags for cfg
    accuracy it must opt in explicitly.
  - **Option B: also try to clear `RUSTC_BOOTSTRAP`.** Benefit: belt and
    suspenders. Risk: rust-analyzer sets `RUSTC_BOOTSTRAP=1` per-invocation
    itself, so an outer empty value is re-set and the attempt is ineffective
    (observed: 9 invocations had it despite our env); and it may degrade the
    `target-data-layout`/`cfg` queries rust-analyzer uses it for. Not worth
    it once Option A removes the only project-controlled `-Z` source.

### D2 - the `[env]` dynamic-linker channel (needs a direction; not closable by env precedence)

- **Today:** cargo applies the project's `[env]` table to the rustc children
  of `cargo metadata`; `DYLD_INSERT_LIBRARIES` (macOS) / `LD_PRELOAD` (Linux)
  therefore runs a project-named dylib. Empty parent values do not stop it
  (arm C) and `force = true` overrides regardless (arm D).
- **Change:** choose one of the options below.
- **Example:** a repo with `[env] DYLD_INSERT_LIBRARIES = { value =
  "./evil.dylib", force = true }` runs `evil.dylib` on first index, with no
  build and no network.
- **Consequence:** depends on the option.

  - **Option A (recommended for this release): ship D1 now, document the
    `[env]` injection as a known residual, and open a follow-up task.**
    Benefit: ships the verified rustflags win immediately without fragile
    machinery; the residual needs a prebuilt dylib committed to the repo,
    which is conspicuous. Risk: a hostile clone with a committed dylib +
    `[env]` still achieves code execution on index until the follow-up lands.
  - **Option B: sandbox the rust-analyzer process at the OS level**
    (`sandbox-exec`/Seatbelt on macOS; seccomp/Landlock or a `bwrap` jail on
    Linux) to deny loading dylibs outside the toolchain prefix and deny
    `exec` of unexpected binaries. Benefit: closes `[env]` injection and the
    whole exec class structurally, not channel by channel. Risk: a large,
    platform-specific effort with its own perf and maintenance cost - its own
    epic, not this task.
  - **Option C: a non-empty sentinel for the linker vars.** Rejected: the
    empty value was already overridden (arm C), and `force = true` overrides
    any parent value (arm D), so this buys nothing and reads as protection
    that is not there.

## Edit map (if D1 is approved; no production code changed in this slice)

- `plugins/rust/src/semantic.rs`, `pinned_env` (`:171`): insert
  `env.insert("CARGO_ENCODED_RUSTFLAGS".into(), String::new());`; extend the
  function's doc comment to state that the empty encoded rustflags outrank
  `.cargo/config.toml` `[build]`/`[target.*] rustflags` (no ticket id in the
  comment). Callers: `semantic::engine` (`:90`) only, via
  `config.env.extend(pinned_env(&toolchain))` (g-mesh `find_callers` on
  `pinned_env` -> one caller, `semantic::engine`).
- `plugins/rust/tests/semantic_runs_no_project_code.rs`, `write_fixture`
  (`:201`): add `[target.<triple>] rustflags = ["-Zcodegen-backend=<marker
  dylib>"]` to the fixture's `.cargo/config.toml` and a marker dylib; the
  existing `fixture.written().is_empty()` assertion (`:393`) then also
  guards channel 2. (g-mesh `find_callers` on `semantic::engine` id
  `c6b72…`: called by this test and by
  `tests/indexing_writes_nothing_and_stays_offline.rs::index_and_check`,
  plus the `main.rs` factory.)
- Unit test in `semantic.rs` `#[cfg(test)]`: assert
  `pinned_env(&toolchain)` contains `CARGO_ENCODED_RUSTFLAGS` = `""`, beside
  the existing `the_server_env_pins_the_toolchain_by_name`.

## Candidate controls (for the fix slice; 6-8, grouped by test binary)

1. **Ctl-RF-int:** delete the `CARGO_ENCODED_RUSTFLAGS` insert in
   `pinned_env`; the extended integration test
   (`--test semantic_runs_no_project_code`) must fail with its
   codegen-backend marker written.
2. **Ctl-RF-unit:** delete the same insert; the new `pinned_env` unit test
   (`--lib`) must fail.
3. **Ctl-RF-empty-vs-absent:** set the insert to a non-empty value instead
   of `""`; the integration test must still fail (only an empty encoded
   rustflags is the "no flags" signal).

(No control for the `[env]` channel in this slice: D2 ships no code fix for
it yet. When D2's direction is chosen, add its control then.)

## g-mesh calls used

- `select_project g-mesh-wt-rel480` (rel480 already indexed; same code as the
  gm572 worktree, which was cold).
- `find_callers pinned_env` -> one caller: `semantic::engine`
  (`plugins/rust/src/semantic.rs:90`).
- `find_callers semantic::engine` (ambiguous across the three plugins) ->
  resolved by id `c6b72de3…` -> callers: the two Rust test files above and
  the `main.rs` factory (excluded as a non-CALLS top-level reference).
