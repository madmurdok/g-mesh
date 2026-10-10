# GM-538: why `target/debug/deps` grows by thousands of files

Slice S1 (diagnosis). Measured 2026-10-09 in `g-mesh-wt-gm538` (branch
`fix/GM-538-test-binary-metadata-hash`, off `release-4.3.0` at `07b8a12`),
starting from an empty `target/`. rustc 1.97.1 (x86_64-apple-darwin),
cargo-nextest 0.9.146, `~/.cargo/config.toml` sets `rustc-wrapper = sccache`.
Load average during the run ranged from 8 to 320 (other agents were building);
timings below are indicative only.

## Verdict

The growth is **not** new test-binary hashes. It is per-codegen-unit object
files (`*.rcgu.o`) that rustc leaves in `target/debug/deps` because the macOS
dev/test default is `split-debuginfo = "unpacked"`. Their file names carry a
per-compilation suffix, so **every recompilation of a workspace crate writes a
complete new set of `.o` files next to the old ones**, and cargo never deletes
the old ones. A plain `touch core/src/lib.rs` + test build adds 8,775 files.
An unchanged repeat of the routine commands adds nothing.

Evidence the main checkout's count is exactly this: its 4,349
`plugin_memory_limit-*` entries = 21 x 207, and one build of that test binary
leaves exactly 207 `.o` files (table below) - i.e. 21 recompilations, not 21
(or 4,349) hashes.

## Per-command before/after (`target/debug/deps`)

Columns: total entries; per extension; `T` = `plugin_memory_limit-*`
(distinct hashes / entries / `.o`); `units` = distinct `<name>-<16hex>`
artifacts; `+files` = new entries vs previous row; `+units` = new hashes.
No row removed any file.

| step | command | total | .o | .d | .rmeta | .rlib | .dylib | exec | T hashes | T entries | T .o | units | +files | +units |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | (empty) | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | | |
| 1 | `cargo build --workspace` | 2,502 | 1,791 | 247 | 217 | 217 | 23 | 7 | 0 | 0 | 0 | 249 | 2,502 | 249 |
| R1.1 | `cargo build --workspace` (again) | 2,502 | 1,791 | 247 | 217 | 217 | 23 | 7 | 0 | 0 | 0 | 249 | 0 | 0 |
| R1.2a | `cargo nextest run -p g-mesh-wire` | 2,649 | 1,910 | 257 | 225 | 225 | 24 | 8 | 0 | 0 | 0 | 259 | 147 | **10** |
| R1.2b | `cargo nextest run --workspace` (builds all tests, filter runs none) | 14,252 | 13,135 | 419 | 279 | 279 | 24 | 116 | 1 | 209 | 207 | 423 | 11,603 | 164 |
| R1.2c | `cargo test -p g-mesh --test plugin_memory_limit --no-run` | 14,751 | 13,619 | 425 | 282 | 282 | 25 | 118 | **2** | 418 | 414 | 429 | 499 | **6** |
| R1.3 | `scripts/check.sh` (fmt + clippy --workspace --all-targets) | 15,580 | 13,619 | 806 | 659 | 349 | 29 | 118 | 3 | 419 | 414 | 812 | 829 | 383 |
| R1.4 | `scripts/test-deps.sh` | 15,580 | 13,619 | 806 | 659 | 349 | 29 | 118 | 3 | 419 | 414 | 812 | 0 | 0 |
| R2.1-R2.4 | identical repeat of 1, 2a, 2b, 2c, 3, 4 (each row) | 15,580 | 13,619 | 806 | 659 | 349 | 29 | 118 | 3 | 419 | 414 | 812 | **0** | **0** |
| 6 | `touch core/src/lib.rs` + nextest workspace test build | 24,355 | 22,394 | 806 | 659 | 349 | 29 | 118 | 3 | 626 | 621 | 812 | **8,775 (all .o)** | **0** |
| 7.0 | throwaway worktree, `cp -c -R` of target | 24,355 | 22,394 | | | | | | 3 | 626 | 621 | 812 | | |
| 7.1 | same nextest build, unchanged sources, new directory | 36,915 | 34,954 | 806 | 659 | 349 | 29 | 118 | 3 | 833 | 828 | 812 | **12,560 (all .o)** | 0 |
| 7.1e | add a `pub fn` to `core/src/lib.rs`, rebuild | 45,690 | 43,729 | | | | | | 3 | 1,040 | 1,035 | 812 | 8,775 | 0 |
| 7.1r | `git checkout --` (revert), rebuild | 54,465 | 52,504 | | | | | | 3 | 1,247 | 1,242 | 812 | 8,775 | 0 |
| 7.2e | edit again, rebuild | 63,240 | 61,279 | | | | | | 3 | 1,454 | 1,449 | 812 | 8,775 | 0 |
| 7.2r | revert again, rebuild | 72,015 | 70,054 | | | | | | 3 | 1,661 | 1,656 | 812 | 8,775 | 0 |

(Blank cells in rows 7.x: unchanged from the row above; only `.o` grew.)

### Which commands add what

- **New hashes (bounded, one-time):**
  - `cargo nextest run -p g-mesh-wire`: 10 units (wire built *without*
    `json-schema`, which core turns on under `--workspace`, plus its
    serde/syn subtree) - feature unification.
  - `cargo test -p g-mesh ...`: 6 units, including a **second hash of
    `plugin_memory_limit`** and of the `g_mesh` lib (`rmcp`, `rmcp_macros`,
    `serde_json` resolve with fewer features under `-p g-mesh` than under
    `--workspace`) - feature unification again.
  - `scripts/check.sh`: 383 units (clippy check-mode `.rmeta`; expected).
  These appear once and are reused on every repeat: R2 added none.
- **Only new `.o` files (unbounded):** every command that *recompiles* a
  workspace crate - a touch, an edit, a revert, or a build in a moved/cloned
  target dir (7.1: fingerprints hold absolute paths, so the clone recompiles
  every workspace crate). Each recompile adds the crate's full CGU set
  (`g_mesh` lib 256, `plugin_memory_limit` 207, a core touch 8,775 in total).
- `scripts/test-deps.sh`: nothing (it runs `rustup component add` and two
  `npm ci`; no cargo build). Run, light.

## Cause, with evidence

1. **Name shape.** `g_mesh-2a4c5fb9045842ff.04bd2lilsayaz3xknluznhudf.18k9hjf.rcgu.o`
   = `<crate>-<metadata hash>.<CGU name>.<per-compilation suffix>.rcgu.o`. All
   objects of one compilation share the third component.
2. **The suffix changes per compilation, the CGU names do not.** After step 6
   the one `plugin_memory_limit-5a398082e5094a9c` hash has 207 CGU names x 2
   suffixes (`04sinx9` from the first build, `0z6pkmn` from the rebuild after
   the touch): every CGU name appears exactly twice. So a recompile is not
   overwriting; it is adding.
3. **Nothing prunes them.** No row in the table removed a single file. Cargo
   tracks `.d`/`.rlib`/`.rmeta`/executables by hash and overwrites them; the
   `.o` files are rustc-side outputs cargo does not know about.
4. **They exist only because of `unpacked`.** A/B (step 8): the same build of
   `plugin_memory_limit` in APFS clones of the 72k-entry target, with
   `CARGO_PROFILE_{DEV,TEST}_SPLIT_DEBUGINFO`:

   | arm | +files | of which .o | +units (new hashes) | other |
   |---|---|---|---|---|
   | unpacked (default) | 468 | 468 | 0 | |
   | packed | 724 | **0** | 252 | 2 `.dSYM` dirs |
   | off | 722 | **0** | 252 | |

   (The 252 new units under packed/off are a one-time full rebuild: the
   split-debuginfo setting is part of the metadata hash.)
5. **Arithmetic fits the main checkout:** 4,349 = 21 x 207.

Not the cause (ruled out): RUSTFLAGS or `CARGO_*` env in `scripts/*.sh`
(none set; `check.sh` and `test-deps.sh` set no flags), g-mesh-bench (builds
only `--release -p g-mesh`, no debug deps), `.cargo/config.toml` (sets only
`[env] G_MESH_HOME`, which is not part of any hash), sccache (workspace crates
are incremental, so it passes through; hashes did not change between repeats).

## Backtraces per arm

Probe crate (a `#[test]` that panics in a `#[inline(never)]` fn),
`RUST_BACKTRACE=1`, fresh target dir per arm:

| arm | user frames show `src/lib.rs:LINE` | deps files (probe) |
|---|---|---|
| unpacked | yes (4 lines) | 30 (25 `.o`) |
| packed | yes (4 lines, `./src/lib.rs:4:9`, `:12:9`, `:11:16`), via the `.dSYM` | 6 |
| off | **no** - only the panic message location and std frames; user frames have no file:line | 5 |

`off` is therefore not acceptable for a test suite that relies on panic
backtraces.

## Exec latency (one `plugin_memory_limit` binary, `--list`, 3 runs each)

| dir entries | arm | real (s) | user/sys | load avg (1 min) |
|---|---|---|---|---|
| 24,355 | unpacked | 0.36 / 0.40 / 0.37 | 0.00 / 0.01 | 59-64 |
| 72,483 | unpacked | 0.56 / 0.93 / 0.91 | 0.00 / 0.00 | 8 |
| 72,739 | packed | 0.59 / 0.79 / 0.52 | 0.00 / 0.01 | 17 |
| 72,737 | off | 0.53 / 0.70 / 0.53 | 0.00 / 0.01 | 53-57 |

`real` with `user 0.00` = waiting, consistent with the prior finding (dyld /
directory scan scales with entries). At equal entry counts the arms are
indistinguishable: latency follows the entry count, not the split mode. The
win from `packed` is that the count stops growing.

## Proposed fix (S2)

**F1 (recommended): `split-debuginfo = "packed"` for dev/test on macOS only,**
via `.cargo/config.toml`:

```toml
[target.'cfg(target_os = "macos")']
rustflags = ["-C", "split-debuginfo=packed"]
```

- Benefit: no `.o` files in `deps` at all (measured: 0 under packed), so the
  directory stops growing; backtraces keep file:line (measured); Linux and
  Windows CI unchanged.
- Risks: (a) S2 must verify that this rustflag wins over the
  `-C split-debuginfo=unpacked` cargo itself passes for the dev profile on
  macOS (rustc takes the last `-C` occurrence; cargo appends rustflags after
  its own args - unverified here); (b) any exported `RUSTFLAGS` replaces
  config `rustflags` wholesale, silently restoring `unpacked`; (c) `dsymutil`
  runs on every link of every test binary/executable - extra link time per
  relink, **not measured cleanly here** (the packed arm's 182 s included a
  full deps rebuild); S2 should measure a touch-and-rebuild under both arms;
  (d) one-time full rebuild (new hashes) and the old unpacked artifacts stay
  until a `cargo clean`.

**F2: `[profile.dev] split-debuginfo = "packed"` in the root `Cargo.toml`.**
Simpler and not overridable by `RUSTFLAGS`, but applies to every platform:
Linux would switch from the default `off` to `.dwo`/`.dwp` packaging (extra
link step via rustc's built-in thorin, changes what the Linux CI job builds);
Windows MSVC is already `packed` (PDB), so no change there.

**Rejected:** `off` (loses file:line in user frames, measured);
`debug = "line-tables-only"` (smaller `.o`, same count); a periodic
`find target/debug/deps -name '*.rcgu.o' -delete` or `cargo clean` threshold
(treats the symptom; that threshold is S6's scope).

**Existing checkouts:** after the fix lands, one `cargo clean` (or deleting
`target/debug/deps/*.rcgu.o`, safe for builds; only loses line info for
already-linked unpacked binaries until they relink).

## Edit map for S2

- F1: `.cargo/config.toml` (65 lines) - append a
  `[target.'cfg(target_os = "macos")'] rustflags = [...]` table after the
  `[env]` table at lines 64-65, with a WHY comment in the file's existing
  style (cite GM-538 and this note).
- F2 instead: root `Cargo.toml` (38 lines) - append `[profile.dev]
  split-debuginfo = "packed"` after the `[workspace]` table at lines 36-38
  (`[profile.test]` inherits dev). No crate manifest has a `[profile]` table
  (checked `core/`, `wire/`, `plugins/*`); profiles are only honoured at the
  workspace root anyway.
- No script changes needed: `scripts/check.sh` and `scripts/test-deps.sh` set
  no flags.

## CI risk

- CI tests on `macos-15-intel`, `macos-15` (aarch64), `ubuntu-22.04`,
  `windows-2022` (MSVC) (`.github/workflows/ci.yml` lines 81-84). CI targets
  are fresh per run, so the growth itself never shows there.
- F1: Linux/Windows unaffected. macOS jobs gain the `dsymutil` per-link cost
  (to measure in S2). If a CI step exports `RUSTFLAGS` (none does today), F1
  silently stops applying.
- F2: Linux jobs change debuginfo packaging (`.dwp`); Windows unchanged.
- Release builds (`release.yml`, `--release`) are not affected by either: the
  release profile has no debuginfo.

## Other findings

- `-p` vs `--workspace` produces a second hash for the same test binary
  (`cargo test -p g-mesh` vs the workspace build) and for `g_mesh_wire`.
  Bounded (one extra set each), but each extra set also accumulates `.o` on
  every recompile under `unpacked`. Not worth fixing separately once F1/F2
  lands.
- Moving or cloning `target/` into another worktree (the control-build
  recipe) recompiles every workspace crate (7.1: 12,560 new `.o`) - under
  `unpacked` that alone adds ~12.5k files to the cloned dir.
- Disk: the measurement worktree's `target/` reached 12 GB after steps 1-6.

## Raw data

Script, per-step listings and timings (`summary.tsv`, `timings.txt`,
`list.*`, `added.*`, `newunits.*`, `bt-*.txt`) are in the session
scratchpad `gm538/` directory.

## Owner decisions (2026-10-09)

- **Placement: "Только macOS, .cargo/config.toml".** The fix sets
  `-C split-debuginfo=packed` for macOS targets only in `.cargo/config.toml`;
  Linux and Windows are unchanged. The A/B above used
  `CARGO_PROFILE_DEV_SPLIT_DEBUGINFO`, so the implementing slice must confirm
  that the rustflags form also leaves 0 `.o` files (cargo passes the profile's
  own `-C split-debuginfo` too; the later flag must win). An exported
  `RUSTFLAGS` replaces config rustflags, which brings the growth back; say so
  where the setting lives.
- **dsymutil cost: "Порог +10%, иначе вернуться".** If `packed` makes an
  incremental test build (touch one core file, rebuild tests) more than 10%
  slower than `unpacked`, the fix is not taken as is and the owner is asked
  again (options: `off`, or periodic pruning of stale `.o`).

## Measurement and final decision (2026-10-09)

`packed` failed the +10% gate (GM-538/S8): an incremental test build after
touching `core/src/lib.rs` relinks 75 test binaries, and `dsymutil` on them
costs 289 s CPU and writes 4.8 GB of `.dSYM`; median user+sys rose x2.30 and
real x1.21 under load. The config change is reverted.

**Owner: "Чистить устаревшие .o скриптом".** `unpacked` stays. A script
removes `*.rcgu.o` files in `target/debug/deps` that no current test or
binary's debug map references, so `file:line` in backtraces is kept, and the
test entry point (`scripts/test-sections.sh`, GM-540) runs it before a run.
A plain `cargo test` outside the script still accumulates objects; the script
is the remedy, not a guarantee.

## Measurement with the prune (GM-538/S16, 2026-10-09)

Fresh worktree at b53bb4d, cold target dir, macOS default `unpacked`.
Section `wire` (`package(g-mesh-wire)`; `--workspace` still builds every test
binary). `exec` = executable files in `deps`. Timings are `/usr/bin/time -p`.

| step | command | total | .o | .d | .rmeta | .rlib | .dylib | exec | du | real / user / sys (s) | load avg (1/5/15) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| a | `cargo build --workspace` | 2,502 | 1,791 | 247 | 217 | 217 | 23 | 7 | 1.5G | 183.5 / 215.3 / 49.5 | 97 / 84 / 75 |
| b | `test-sections.sh run wire` | 13,128 | 12,035 | 411 | 271 | 271 | 23 | 117 | 6.5G | 424.7 / 1085.6 / 195.3 | 652 / 403 / 222 |
| c | `scripts/check.sh` | 13,961 | 12,035 | 794 | 650 | 338 | 27 | 117 | 6.7G | 214.1 / 174.6 / 43.8 | 276 / 382 / 258 |
| d1 | touch `core/src/lib.rs` + `run wire` | 22,499 | 20,573 | 794 | 650 | 338 | 27 | 117 | 6.6G | 130.8 / 262.4 / 116.5 | 340 / 361 / 266 |
| d2 | same | 22,499 | 20,573 | | | | | | 6.6G | 154.0 / 260.5 / 116.8 | 385 / 354 / 277 |
| d3 | same | 22,499 | 20,573 | | | | | | 6.6G | 119.3 / 274.8 / 123.6 | 282 / 321 / 274 |
| e | control: touch + plain `cargo nextest run` (no prune) | 31,457 | 29,531 | | | | | | 6.6G | 151.8 / 279.7 / 126.4 | 385 / 332 / 285 |
| e' | `scripts/prune-stale-objects.sh` | 13,541 | 11,615 | | | | | | 6.4G | 25.5 / 1.1 / 5.6 | 419 / 346 / 292 |

(Blank cells: unchanged from the row above; only `.o` moved.) A dry run after
each of d1-d3 reported 11,615 referenced and 8,958 stale objects: the prune
runs *before* cargo, so the set a run makes stale stays until the next run.

Exec latency, one `plugin_memory_limit` binary, `--list`, 5 runs each:

| deps entries | state | real (s) | median | user / sys | load avg (1 min) |
|---|---|---|---|---|---|
| 22,501 | bounded with prune (after d3) | 0.40 / 0.54 / 0.43 / 0.39 / 0.41 | 0.41 | 0.01 / 0.01 | 260 |
| 31,459 | after control e, before prune | 0.52 / 0.52 / 0.53 / 0.62 / 0.52 | 0.52 | 0.01 / 0.01 | 389 |
| 13,543 | after e' (one set) | 0.41 / 0.42 / 0.41 / 0.40 / 0.17 | 0.41 | 0.01 / 0.01 | 428 |

Machine: load average 250-650 for the whole run (other work on the machine);
`real` at `user 0.01` is waiting, not computing, so latency numbers are
indicative only.

**Conclusion.** With the prune in `test-sections.sh run`, `deps` is bounded:
three touch cycles stayed at 22,499 entries (one live set plus the one stale
set the previous run left, ~8,960 `.o`), while the control without the prune
grew by +8,958 in one cycle, and a manual prune dropped it by 17,916 to
13,541, matching the post-check baseline. Exec latency was 0.41 s median at
the bounded count against 0.52 s after one unpruned cycle (+9k entries), under
heavy load; the 13.5k state did not measure faster than 22.5k here, so the
latency gain from the prune is within the load noise at these counts and the
main win is that the count stops growing.

**Recommended `cargo clean` threshold for the slice-task skill (pending owner
approval):** run `cargo clean` when `target/debug/deps` holds more than
**25,000 entries**: ~14,000 after a clean build, a workspace test build and
`scripts/check.sh`, plus the one stale core set (~9,000) the prune leaves
between runs, plus ~2,000 margin. Above it, objects are accumulating outside
the script (plain `cargo test`/`nextest`, feature-unification hashes).
