# GM-587: close the `.cargo/config.toml` `[env]` dynamic-loader channel that runs project code when rust-analyzer indexes a project

Status: design (slice S5), awaiting owner decisions D1-D3.
Branch: `fix/GM-587-cargo-env-loader-vars`.

## Question

GM-572 D2 found a channel its env-precedence fix (`pinned_env`, ADR 0032)
does not close. Cargo applies a project's `.cargo/config.toml` `[env]` table
to the `rustc` children of `cargo metadata` (and to a `rustc` wrapper). An
`[env]` entry for a dynamic-loader variable - `DYLD_INSERT_LIBRARIES` on
macOS, `LD_PRELOAD` on Linux - therefore loads a project-supplied dylib into
those children when rust-analyzer indexes a cloned project, with no build
script, no proc macro and no network. `pinned_env`'s own doc comment records
the gap: "The config's `[env]` table reaches `rustc` by another route and is
not covered." Env precedence cannot close it: cargo re-applies the config
`[env]` to the children after inheriting the server's environment, and
`force = true` overrides any parent value outright.

This slice decides the mechanism that closes it, compares the candidates on
per-OS reach, cost, what they break and what they leave open, and (AC3)
enumerates the channels by which indexing can still run project code after
the chosen fix.

## Method

Reused GM-513/GM-572's harness, driven over three abstract fixtures in
`scratchpad/gm587/` (`ra*.sh`, `lsp_drive.py`, `fx/`, `fxr/`, `fxp/`,
`bin/`, `*-summary.txt`, `MARK.*`). `lsp_drive.py` is a minimal LSP client:
it opens one fixture as the workspace, applies the plugin's own init options
(`checkOnSave=false`, `cargo.buildScripts.enable=false`,
`procMacro.enable=false`), waits for rust-analyzer's `quiescent` status and
asks one `definition`. Every arm runs under `pin()`, a shell replica of the
exact environment `pinned_env` builds (`plugins/rust/src/semantic.rs:182`):
`CARGO_NET_OFFLINE=true`, empty `CARGO_ENCODED_RUSTFLAGS`,
`CARGO_UNSTABLE_HOST_CONFIG=false`, `RUSTUP_TOOLCHAIN`, the toolchain's own
`RUSTC`/`CARGO_BUILD_RUSTC`/`CARGO`, and the four wrapper variables emptied.
Server: `rustup which rust-analyzer`, toolchain `stable-x86_64-apple-darwin`
(macOS host). The host was under heavy load (load average 14-33 over an
8-day uptime); timings are indicative only, and quiescence - not a timer -
gated each measurement.

The attack fixture (`fx`) carries a `.cargo/config.toml` whose `[env]` sets
the loader variable to a **marker dylib** with `force = true`. The marker
dylib is a test library whose only effect is a load-time constructor that
appends one line (pid, ppid, sandbox state, argv) to a shared `MARK` file;
it writes nothing else and executes no payload. A non-empty `MARK` after an
arm therefore means project code ran inside that process. (Library source,
the config and the per-arm scripts live in `scratchpad/gm587/`; they are not
reproduced here.) Each arm clears `MARK` and the fixture's `target/` before
running, and copies `MARK` aside as `MARK.<arm>`.

## Results

All arms run the plugin's own init options and `pin()` environment; "fired"
means the marker ran in one or more children.

| Arm | Mechanism tested | Marker | Reading |
|---|---|---|---|
| R0 baseline | none (attack fixture) | **fired**: ~27 `rustc` + 2 `rustup` children, all `sandboxed=0` | `[env] force=true` injects despite the full `pinned_env` scrub. Vulnerability confirmed on macOS. |
| R11 extraEnv-only | rust-analyzer `cargo.extraEnv` sets the loader var to `""` | **fired**: ~27 `rustc` | RA neutralizing the var in its own child's environment does nothing: cargo re-applies the config `[env]` (`force`) to the `rustc` grandchildren. Config-level fix via RA settings is dead. |
| R1 tramp_restrict | point `RUSTC`/`CARGO_BUILD_RUSTC` at a restricted-entitlement trampoline that re-execs rustc | partial: `rustc` clean, 2 `rustup which cargo` still fired | A restricted-entitlement binary makes dyld strip `DYLD_*` for *that* exec, but rust-analyzer's own `rustup which cargo` calls still inherit and load the var. Incomplete, and macOS-only. |
| R4 / R4b / R4c / R4d seatbelt (deny) | `sandbox-exec` wraps the whole RA tree, `deny file-map-executable` on the project subpath | **none** across 4 repeats | The loader var is still set, but the dylib cannot be mapped executable, so the load fails. (R4 in the first run showed one stray `rustc` under load average 18 - a non-quiescent carry-over from the prior arm; four clean repeats rule it out.) |
| R9 allowlist x2 | `sandbox-exec`, `deny file-map-executable` globally, allow only `/System`, `/usr/lib`, Apple/Developer, Xcode, the rustup toolchain prefix | **none** (2 repeats) | Default-deny mapping with a toolchain/system allowlist also closes it, and is the safer profile shape (a new injection path outside the allowed prefixes is denied by default). |
| R10 extraEnv + wrapper env | R11's `cargo.extraEnv` plus a parent-env override | none | Confounded by R11's failure; carries no independent signal. |
| R3 ratoml_extraEnv | fixture with a `rust-analyzer.toml` `[cargo.extraEnv]` loader var, no `.cargo/config.toml` | none | Side check: a project-local `rust-analyzer.toml` `cargo.extraEnv` did not reach the probe children in this RA version. Noted as a separate surface under AC3, not relied on. |

Two load-bearing facts for the decision:

1. **The injection survives every environment-level defense** (R0, R11, R1):
   empty parent values, RA `extraEnv`, and partial wrapper substitution all
   leave at least one child loading the dylib. `force = true` is the reason.
2. **The OS sandbox closes it at the mechanism** (R4 x4, R9 x2): denying the
   dylib map makes the load fail regardless of how the var got set, and
   regardless of which child (rustc, rustup, cargo) carries it.

## Mechanism comparison

### (a) OS sandbox of the rust-analyzer process tree

Wrap the server launch so the sandbox confines rust-analyzer and every
child it spawns (rustc probes, rustup, cargo, and - if ever enabled - build
scripts and the proc-macro server).

- **Reach, macOS:** `sandbox-exec -f <profile>` (Seatbelt). Proven: R4 and
  R9 blocked the load across six repeats. `file-map-executable` is the hook
  (denying `dlopen`/mmap-exec of the project dylib). Reach, Linux:
  equivalent with Landlock (kernel >= 5.13; path-based, no privilege) for
  the filesystem denial, or a `bwrap` user-namespace jail where Landlock is
  unavailable; neither was measured here (macOS host).
- **Cost:** macOS arm ran at ~10-31s, within baseline noise. The cost is
  engineering: a per-OS profile and a launch wrapper, plus keeping the
  allowlist correct as toolchains move. `sandbox-exec` is deprecated-but-
  present on every supported macOS; Landlock/`bwrap` add a capability probe
  and a fallback on Linux.
- **What breaks:** an allow-default, deny-project profile (R4) breaks
  nothing observed. A default-deny allowlist (R9) must allow the toolchain
  prefix, system libs, the C toolchain/linker (Xcode / Developer on macOS),
  `CARGO_HOME`, the sccache dir if used, and the proc-macro/temp dirs - miss
  one and indexing fails to start. Denying `file-map-executable` for project
  paths also blocks loading project proc-macro and build-script dylibs, but
  the plugin already disables both for indexing, so that is not a
  regression.
- **Residual channels:** the sandbox contains the *effects* of injected code
  (file writes, exec, and - if the profile denies it - network), so it
  neutralizes not just this `[env]` channel but the wrapper and future
  build-script/proc-macro classes too. It does not stop pure in-process
  computation inside a child, but with no writable project paths, no exec and
  no network that computation cannot persist or exfiltrate.
- **SIP note:** macOS SIP strips `DYLD_*` only when the target binary is
  SIP-protected or carries a restricted entitlement. rustup's toolchain
  binaries live under `~/.rustup` and are user-installed, so SIP does **not**
  strip the var for them - R0 proves the injection reaches them. SIP is not a
  defense here. (R1 shows a restricted-entitlement trampoline does trigger
  the strip, but only for the one wrapped exec, not for rustup's own
  children.)

### (b) Refuse semantic indexing when the effective cargo config sets an `[env]` loader var

Before building the LSP engine, discover the cargo config that *would* apply
and, if its merged `[env]` table names a dynamic-loader variable, return
`Err` from the factory. `LazyEngine` already turns a factory `Err` into
structural-only for the process lifetime with a logged reason
(`plugins/sdk/src/semantic.rs:224`, the `failed`/`start_failure` path), so no
new plumbing is needed - the degradation path exists.

- **Reach:** OS-independent; a pure static check, deterministic, no sandbox.
- **Cost:** the check must reproduce cargo's config resolution exactly, or it
  either misses a hostile config (false negative = still vulnerable) or
  over-refuses. That means: walk from the workspace dir to the filesystem
  root merging each `.cargo/config.toml` (and the legacy extensionless
  `.cargo/config`); then `$CARGO_HOME/config.toml` (default `~/.cargo`);
  honour every `include` key and its transitive includes; and inspect each
  merged `[env]` table for the loader keys `DYLD_INSERT_LIBRARIES`,
  `DYLD_LIBRARY_PATH`, `DYLD_FRAMEWORK_PATH`, `LD_PRELOAD`,
  `LD_LIBRARY_PATH`, `LD_AUDIT` (both the `"val"` and `{ value = , force = }`
  forms). Getting the merge wrong in either direction is the main risk.
- **What breaks:** nothing in the process tree - the project just indexes at
  the structural tier. But a *legitimate* project that sets, say,
  `LD_LIBRARY_PATH` in `[env]` for an FFI crate is downgraded too; the
  false-positive rate depends on how broad the key list is. Scoping the
  refusal to the macOS/Linux loader-hijack keys (`*_INSERT_LIBRARIES`,
  `LD_PRELOAD`, `LD_AUDIT`) keeps it narrow; adding `LD_LIBRARY_PATH` widens
  both safety and false positives.
- **Residual channels:** closes only the `[env]` loader-var channel. It does
  not touch the wrapper channel, `rust-analyzer.toml` surfaces, or the
  future build-script/proc-macro classes (see AC3). Defense in depth, not a
  structural fix.

### (c) Others considered

- **Sanitized `CARGO_HOME` + `--config` override.** A clean `CARGO_HOME`
  removes the *global* `$CARGO_HOME/config.toml` `[env]` channel but not the
  project-local `.cargo/config.toml`, which is the actual threat. Passing
  `--config 'env.DYLD_INSERT_LIBRARIES=""'` via RA's `cargo.extraArgs` is an
  env-level neutralization, and R11 shows env-level neutralization loses to
  `force = true`. Rejected as a primary fix; a clean `CARGO_HOME` is still
  worth setting as hardening against the global-config variant.
- **Avoid the rustc/cargo-metadata probe entirely.** Feed rust-analyzer a
  prebuilt project model (`linkedProjects` / a `rust-project.json`) and a
  precomputed sysroot so it runs no `cargo metadata` and no `rustc --print`.
  But computing that model requires running `cargo metadata` once - which is
  the injection. Chicken-and-egg: the bootstrap probe still has to run under
  (a) or (b). A refinement, not a standalone mechanism.
- **Env precedence / non-empty sentinel.** Already disproven (R0, R11,
  GM-572 arm D): `force = true` overrides any parent value. Rejected.

## AC3 - remaining channels by which indexing can run project code

After the chosen fix, these are the channels through which a cloned project
can still get code to run (or be loaded) during indexing. Mechanism (a)
contains all of them; mechanism (b) alone closes only the first.

1. **`.cargo/config.toml` `[env]` loader var** - this slice's channel.
   Closed by (a) and by (b).
2. **`rustc` wrapper via config** - `[build] rustc-wrapper` /
   `rustc-workspace-wrapper`. `pinned_env` empties the four wrapper env vars
   and env outranks config for these keys (ADR 0032), so this is believed
   closed *by precedence*; but it is the same family as the `[env]` channel
   and should be re-probed, since it names a project binary cargo would run.
3. **`rust-analyzer.toml` / `.rust-analyzer.toml` project-local config** -
   `cargo.extraEnv`, `cargo.buildScripts.*`, `check.overrideCommand`,
   `cargo.extraArgs`, `cargo.targetDir`, `cargo.buildScripts.overrideCommand`.
   RA reads project-local config files; `overrideCommand` runs an arbitrary
   command. The plugin passes its own init options, but whether those
   override a project-local `rust-analyzer.toml` for every one of these keys
   is unverified (R3 checked only `extraEnv` and found it did not reach the
   probes in this RA version). A distinct surface, worth its own probe.
4. **Build scripts and proc macros** - disabled by the plugin's init options
   today, so dormant; they become live code-exec channels the moment either
   is enabled, and only (a) contains them structurally.
5. **`[target.*] runner` / `linker`** - a project-named runner or linker
   binary cargo would invoke; not exercised during a metadata+check index
   with linking off, but present in the config surface.
6. **`[build] rustflags` / `-Zcodegen-backend` / `-Zcrate-attr`** - closed by
   GM-572 D1's empty `CARGO_ENCODED_RUSTFLAGS` (ADR update), noted here for
   completeness.

Only (a) closes 1-5 as a class. (b) closes 1 and leaves 2-5 to precedence or
to being dormant.

## Recommendation

Pursue the **OS sandbox (a)** as the structural fix, since it is the only
mechanism that closes the whole exec/load class (AC3 1-5) and it is the one
the experiment proved closes this channel (R4 x4, R9 x2). Because the
sandbox is a per-OS effort, split it into follow-up tasks per the owner rule
"Дизайн решает, потом делим": a macOS Seatbelt task (allowlist profile, the
R9 shape) and a Linux Landlock-with-`bwrap`-fallback task.

Ship **mechanism (b), the refuse-to-structural pre-check, now**, as the
interim shield and permanent defense-in-depth. It is small, OS-independent,
reuses the existing `LazyEngine` degradation path, and closes the one proven
channel immediately while the sandbox follow-ups land. It degrades safely
(structural index, logged reason) rather than executing project code, which
is the correct bias for an indexer.

This is the GM-572 D2 direction made concrete: D2 Option A deferred the
`[env]` channel to a follow-up; this slice *is* that follow-up and proposes
(b) now + (a) as the epic.

## Owner decisions

### D1 - ship the refuse-to-structural `[env]` loader-var pre-check now (recommended)

- **Today:** `engine()` builds the rust-analyzer tier unconditionally; a
  project's `.cargo/config.toml` `[env]` loader var then runs a project dylib
  during the first index (R0).
- **Change:** before building the engine, resolve the effective cargo config
  and, if its merged `[env]` names a dynamic-loader variable, return `Err`
  from the factory so the plugin stays structural-only for the process, with
  a logged reason.
- **Example:** a clone whose `.cargo/config.toml` has
  `[env] DYLD_INSERT_LIBRARIES = { value = "./x.dylib", force = true }`
  indexes at the structural tier and logs the refusal; no dylib loads.
- **Consequence / benefit:** closes the proven channel immediately,
  portably, with no new runtime machinery. **Risk:** the config-resolution
  walk must match cargo's merge exactly or it under- or over-refuses; and a
  legitimate project using a loader var in `[env]` loses its semantic tier
  (mitigated by scoping the key list to the hijack vars).

### D2 - adopt the OS sandbox as the structural fix, split per OS (recommended)

- **Today:** nothing confines the rust-analyzer process tree; any channel in
  AC3 1-5 that reaches a child runs unconfined.
- **Change:** launch rust-analyzer inside an OS sandbox that denies loading
  executables outside the toolchain/system prefixes and denies exec of
  unexpected binaries; implement per OS in follow-up tasks (GM-587-a macOS
  Seatbelt, GM-587-b Linux Landlock/`bwrap`).
- **Example:** the same hostile clone indexes with the dylib map denied (R4,
  R9); the load fails and indexing proceeds at the semantic tier.
- **Consequence / benefit:** closes AC3 1-5 as a class, not channel by
  channel, and keeps the semantic tier working on hostile repos. **Risk:**
  per-OS engineering and maintenance (profile/allowlist correctness as
  toolchains move; `sandbox-exec` is deprecated; Landlock needs a capability
  probe and a `bwrap` fallback).

### D3 - key list for the D1 pre-check (needs a direction)

- **Today:** no list exists.
- **Change:** pick the loader keys the pre-check treats as hostile.
- **Example:** narrow = `DYLD_INSERT_LIBRARIES`, `DYLD_FRAMEWORK_PATH`,
  `DYLD_LIBRARY_PATH`, `LD_PRELOAD`, `LD_AUDIT`; broad also adds
  `LD_LIBRARY_PATH`.
- **Consequence:** narrow = fewer false positives, misses a
  `LD_LIBRARY_PATH`-based hijack; broad = safer, downgrades legitimate FFI
  crates. **Benefit of narrow:** keeps the semantic tier for common FFI
  projects. **Risk of narrow:** a `LD_LIBRARY_PATH` injection path stays open
  until the sandbox lands. (Recommended: narrow, since the sandbox (D2) is
  the backstop for the rest.)

## Edit map

For D1 (the pre-check); D2's sandbox work is scoped to its own per-OS tasks
and not detailed here.

- `plugins/rust/src/semantic.rs`, **new fn** `cargo_env_loader_var(root: &Path)
  -> Option<(String, PathBuf)>` (place beside `pinned_env`, ~`:207`):
  resolves the effective cargo config for `root` and returns the first
  `[env]` loader key found and the config file it came from. Resolution:
  walk `root` -> filesystem root collecting `.cargo/config.toml` (then
  `.cargo/config`); then `$CARGO_HOME/config.toml` (default `~/.cargo`);
  follow `include` keys transitively; parse each `[env]` table (both scalar
  and `{ value, force }` forms) against the D3 key list. Prefer a small TOML
  read over a shell to cargo.
- `plugins/rust/src/semantic.rs`, **`engine`** (`:91`, caller of
  `pinned_env` - the only one, per `find_callers`): after `Toolchain::detect`
  and before `LspBridge::new`, call `cargo_env_loader_var(root)`; on `Some`,
  `return Err(anyhow!( ... ))` naming the key and file, so `LazyEngine`
  (`plugins/sdk/src/semantic.rs:224`) logs it and keeps the plugin structural
  -only. Extend `engine`'s doc comment to state the invariant (present
  tense, no ticket id): "A project `.cargo/config.toml`/`CARGO_HOME` config
  whose `[env]` sets a dynamic-loader variable is refused to the structural
  tier, because cargo applies that table to the `rustc` children of
  `cargo metadata` and `force = true` defeats any environment override."
  Link ADR 0032/0034 at the decision site, not per function.
- `plugins/rust/src/semantic.rs`, `pinned_env` doc comment (`:177`): replace
  "The config's `[env]` table reaches `rustc` by another route and is not
  covered." with a pointer that the `[env]` loader-var case is handled by
  the refusal in `engine` (the other `[env]` entries remain uncovered by
  precedence and are the sandbox's scope).
- `docs/adr/`: new ADR recording D1 (+ D2 direction) as the follow-up to ADR
  0032/0033; link it from the module doc.

No other call sites: `find_callers(pinned_env)` -> `engine` only;
`find_callers(semantic::engine id c6b72…)` -> the two integration tests plus
the `main.rs` factory reference (non-CALLS, excluded).

## Proposed tests and controls

Tests extend the existing fixture harness in
`plugins/rust/tests/semantic_runs_no_project_code.rs` (`write_fixture`
`:247`, `build_marker_dylib` `:221`, `Fixture::written()` `:175`), which
already indexes a hostile fixture through the real `semantic::engine` and
asserts no marker was written.

- **AC1 (fixture fires no marker):** add to the fixture's
  `.cargo/config.toml` an `[env]` loader entry
  (`DYLD_INSERT_LIBRARIES`/`LD_PRELOAD` per OS) set to the existing marker
  dylib with `force = true`. With D1 in place `engine()` refuses to the
  structural tier, so the server never starts and `fixture.written()` stays
  empty - the existing assertion (`:~430`) now also guards this channel.
  Assert additionally that the refusal reason was logged / the tier is
  structural, so the test cannot pass by the server merely failing to start
  for an unrelated reason.
- **AC2 (mechanism disabled -> marker fires):** this is the control, below.
- **Unit test** in `semantic.rs` `#[cfg(test)]` beside
  `the_server_env_pins_the_toolchain_by_name` (`:501`): build a temp dir
  with a `.cargo/config.toml` `[env]` loader var and assert
  `cargo_env_loader_var(dir)` returns `Some` with that key; and a sibling
  with a benign `[env]` var asserts `None`.
- **Discovery unit tests:** parent-dir config (var in `../.cargo/config.toml`)
  -> `Some`; `include`-d file carrying the var -> `Some`;
  `$CARGO_HOME/config.toml` var -> `Some`; `{ value, force=false }` and
  scalar forms both -> `Some`.

Controls (6-8, grouped by test binary; the verify slice builds each in a
throwaway worktree and requires the named test to fail):

1. **Ctl-env-int:** remove the `Err` return in `engine()` (keep the
   detection call). `--test semantic_runs_no_project_code` must fail with the
   loader marker written.
2. **Ctl-env-detect:** make `cargo_env_loader_var` always return `None`. Same
   integration test must fail (marker written).
3. **Ctl-env-unit:** make `cargo_env_loader_var` always return `None`; the
   `semantic.rs` `--lib` unit test for the loader key must fail.
4. **Ctl-env-parentdir:** restrict discovery to `root` only (drop the
   parent-dir walk); the parent-dir `--lib` discovery test must fail
   (proves the walk, not just same-dir, is load-bearing).
5. **Ctl-env-include:** skip `include` resolution; the `include` `--lib` test
   must fail.
6. **Ctl-env-cargohome:** skip `$CARGO_HOME/config.toml`; that `--lib` test
   must fail.
7. **Ctl-env-force-form:** parse only the scalar `[env]` form, not
   `{ value, force }`; the `{ value, force }` `--lib` test must fail (the
   proven-hostile form must be detected).

Controls 1-2 land in the integration binary, 3-7 in `--lib`; group by binary
for the build. Dry-run the "did it fail" check on the first control.

## g-mesh calls used

- `select_project g-mesh-wt-gm587` (the task worktree; its index was cold /
  structural-only during this slice, so caller results carried
  `provenance: semanticTier absent` - noted below).
- `find_definition pinned_env` -> `semantic::pinned_env`
  (`plugins/rust/src/semantic.rs:181`); source and doc comment returned
  inline (the doc comment is the primary evidence that `[env]` is the known
  gap).
- `get_file_outline plugins/rust/src/semantic.rs` -> located `engine`
  (`:90`), `pinned_env` (`:182`), the `tests` module and its env-pinning
  tests (`:501`+), used for the edit map and test placement.
- `find_callers pinned_env` -> one caller, `semantic::engine`.
- `find_callers semantic::engine` (id `c6b72de3…`) -> callers
  `index_and_check`
  (`tests/indexing_writes_nothing_and_stays_offline.rs:255`) and the
  `semantic_runs_no_project_code.rs` test (`:424`), plus an excluded
  non-CALLS reference in `plugins/rust/src/main.rs` (the factory). Because
  the index was structural-only, this carried a `provenance` hint; the
  callers match GM-572's independently and the `main.rs` factory was
  confirmed by reading `main.rs:22`.
- `find_references engine` returned `nameAmbiguous` (engine is a field name
  across many structs); re-queried by the rust function's id, as above.
