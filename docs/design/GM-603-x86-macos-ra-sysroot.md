# GM-603: rust-analyzer fails to load std on the x86_64 macOS CI runner

Status: diagnosis (S1). Found in GM-584.

## Captured error

Release run 38441041236 (v4.7.0), attempt 1, job 115447445911
`tests / tests (x86_64-apple-darwin)`, image `macos-15` 20260824.0482.1
(`macos-15-intel` runner), toolchain 1.97.1. Test
`plugin_memory_limit::a_real_rust_analyzer_over_its_memory_limit_is_suspended_and_structural_work_continues`,
plugin stderr, verbatim (one of three identical lines, 17:59:33.2 / 33.8 /
34.4; the same line again at 18:00:06 in the daemon log of
`the_generated_mcp_instructions_reflect_a_real_suspended_rust`):

```
[rust] semantic tier: /Users/runner/.rustup/toolchains/1.97.1-x86_64-apple-darwin/bin/rust-analyzer (rust-analyzer 1.97.1 (8bab26f4 2026-07-14), toolchain 1.97.1-x86_64-apple-darwin, cargo offline: dependencies not in the local cargo cache stay unresolved until `cargo fetch`)
[rust] rust-analyzer: 2026-10-13T17:59:33.246112Z ERROR `cargo metadata` failed on `/Users/runner/.rustup/toolchains/1.97.1-x86_64-apple-darwin/lib/rustlib/src/rust/library/Cargo.toml` : Failed to run `cd "/Users/runner/.rustup/toolchains/1.97.1-x86_64-apple-darwin/lib/rustlib/src/rust/library" && CARGO_RESOLVER_LOCKFILE_PATH="/var/folders/20/jp1_0n3n7kndh6rnbqb5344m0000gn/T/rust-analyzer91d0-1/Cargo.lock" RUSTUP_AUTO_INSTALL="0" __CARGO_TEST_CHANNEL_OVERRIDE_DO_NOT_USE_THIS="nightly" "/Users/runner/.cargo/bin/cargo" "metadata" "--format-version" "1" "--manifest-path" "/Users/runner/.rustup/toolchains/1.97.1-x86_64-apple-darwin/lib/rustlib/src/rust/library/Cargo.toml" "--filter-platform" "x86_64-apple-darwin" "-Zlockfile-path" "-Zunstable-options"`
```

What the log does **not** carry: cargo's own stderr. rust-analyzer logs the
error with `{e}` (`crates/project-model/src/sysroot.rs`, `load_workspace`),
which prints only the outermost anyhow context, "Failed to run ...". The
underlying cargo message is lost, in CI and in production alike.

Scope of the evidence: this is the only occurrence. Searched every failed
`tests (...)` job of 9 failed CI/release runs since 4.4.0 (15 job logs):
0 other hits. Passing jobs print no plugin stderr (nextest shows stderr only
for failures), so whether std loaded in a passing job is not observable from
the logs. Attempt 2 of the same run, same image and toolchain, passed - the
150 MB limit trips only with std loaded (GM-584: ~560 MB vs ~45 MB), so std
loaded there.

## Facts established

1. **CI installs no `rust-src`.** `.github/workflows/ci.yml` installs the
   toolchain with `--profile minimal` (rustc, cargo, rust-std: "downloading 3
   components" in the log) and then only `rust-analyzer` (lines 494-498).
   `release.yml` reuses this job through `workflow_call`.
2. **rust-analyzer installs `rust-src` itself, over the network.**
   `Sysroot::discover` -> `discover_rust_lib_src_dir_or_add_component`
   runs `rustup component add rust-src` when the source dir is missing
   (rust-analyzer `crates/project-model/src/sysroot.rs`). The plugin does
   not configure `cargo.sysroot`, so RA takes this path
   (`RustLibSource::Discover`, `workspace.rs`). `CARGO_NET_OFFLINE` does not
   reach rustup. So in CI the first test that starts a real RA downloads and
   unpacks rust-src mid-suite, inside the server's process tree, while other
   tests start, suspend (SIGSTOP) and kill their servers. In production the
   same path means a user without rust-src gets a silent download, which
   ADR 0033 ("semantic tiers reach no network") says must not happen.
3. **The command itself is fine with a complete rust-src.** Reproduced
   locally (x86_64 macOS, rustc 1.97.1): the exact command above, with the
   plugin's full `pinned_env` (`CARGO_NET_OFFLINE=true`, empty
   `CARGO_ENCODED_RUSTFLAGS`, `CARGO_UNSTABLE_HOST_CONFIG=false`,
   `RUSTUP_TOOLCHAIN`, `RUSTC`, `CARGO`, empty wrappers) and an **empty**
   `CARGO_HOME`: exit 0, 106 kB of metadata. rust-src's `library/` ships
   `.cargo/config.toml` replacing crates-io with `vendor/` (30 crates), so
   offline mode and an empty registry cache are ruled out as causes.
4. **Concurrent installs do not corrupt the tree** (as far as tested).
   Throwaway `RUSTUP_HOME`, 1.97.1 minimal, 6 trials of 4 concurrent
   `rustup component add rust-src`: one wins, three exit 1 ("could not rename
   'downloaded' file ... .partial"), the tree is complete (3483 files) and
   cargo metadata exits 0 every time.
5. **A killed install leaves rustup inconsistent.** Killing the add with
   SIGKILL at 1-3 s left nothing; at 4 s it left a complete tree on disk
   that rustup does not list as installed, and every later
   `rustup component add rust-src` then fails ("detected conflict:
   'lib/rustlib/src/rust/library/.cargo/config.toml'"). The tree appeared
   all at once (0 -> 3483 files) in every trial, so a half-unpacked tree was
   not observed.

## Cause

Named with confidence: the sysroot load depends on rust-analyzer adding
`rust-src` at run time, because CI never installs it and the plugin lets RA
auto-install. Everything that can go wrong with that install (network,
concurrent rustup runs from parallel tests, a test killing or suspending the
server's process tree mid-install, rustup's resulting inconsistent state)
lands as "std not loaded" for the rest of the job.

Not proven: which of those broke attempt 1. The exact mechanism needs
cargo's stderr, which RA discards. Three local mechanisms were tried
(offline/empty cache: ruled out; concurrent adds: no corruption in 6/6;
SIGKILL mid-install: inconsistent rustup state, but metadata still loads).
The diagnostic step below captures it if it happens again before the fix.

## Fix options

| # | Option | Benefit | Risk |
|---|---|---|---|
| A | CI: `rustup component add rust-analyzer rust-src` | Removes the run-time install from CI: std loads deterministically on every runner; one line. | Hides the production gap: users without rust-src still get RA's silent download (ADR 0033 breach) or a no-std tier with no explanation. |
| B | Plugin: pass `cargo.sysroot` = the pinned toolchain's sysroot (`rustc --print sysroot`) in `initializationOptions` | RA takes `RustLibSource::Path`, which never runs `rustup component add`: no network from the semantic tier (ADR 0033 holds); sysroot pinned to the same toolchain as `RUSTC`/`CARGO`. | Without rust-src, std is now never loaded (instead of downloaded); needs B2 to tell the user. A user-set `cargo.sysroot`/`RUST_SRC_PATH` keeps working (RA still honours `RUST_SRC_PATH` in `discover_rust_lib_src_dir`). |
| B2 | Plugin: at engine start, if `<sysroot>/lib/rustlib/src/rust/library/Cargo.toml` is missing, log one line: "std not loaded: rust-src is not installed for toolchain X; run `rustup component add rust-src --toolchain X`" (and surface it in the MCP instructions/status if a channel exists) | The limitation is visible instead of silent; matches how the tier already announces "cargo offline". | One more stat at start; the wording becomes a contract that tests pin. |
| C | Keep RA's auto-install, serialize it (pre-install rust-src from the plugin before starting RA) | Users get std without a manual step. | Explicit network access from a plugin, contradicting ADR 0033; still racy across daemons. Rejected. |
| D | Documented limitation only (README) | No code. | CI stays nondeterministic; production stays silent. Rejected. |

## Recommendation

**A + B + B2.** A makes CI deterministic; B removes the network path that
caused the nondeterminism in the first place and is an ADR 0033 fix in its
own right; B2 turns the remaining case (no rust-src) into a documented,
surfaced limitation. Keep the diagnostic step for one release, then drop it.

## Edit map

- `.github/workflows/ci.yml` 494-498, step "Install rust-analyzer": add
  `rust-src` to the `rustup component add` line; extend the comment above
  (481-493) with why (GM-603: otherwise RA installs it mid-suite).
- `.github/workflows/ci.yml` after 886, step "rust-src state and the sysroot
  cargo metadata": the diagnostic step (separate commit on this
  branch, not pushed). Drop it once A+B have run green for a release.
- `plugins/rust/src/semantic.rs`
  - `struct Toolchain` 219-231: add `sysroot: Option<PathBuf>`.
  - `Toolchain::detect` 232-255: fill it from
    `rustup run <pin> rustc --print sysroot` (or the resolved `rustc
    --print sysroot`), run in `neutral_dir` like the other rustup calls;
    the non-rustup branch uses `on_path("rustc")`.
  - `engine` 105-140: after `config.env.extend(pinned_env(&toolchain))`
    (line 127), when `toolchain.sysroot` is `Some`, insert
    `cargo.sysroot = <path>` into `config.initialization_options` (merge
    into the existing `cargo` object; do not overwrite a `sysroot` key the
    manifest or a test already set). B2: if
    `<sysroot>/lib/rustlib/src/rust/library/Cargo.toml` does not exist and
    `RUST_SRC_PATH` is unset, `log_line!` the rust-src hint.
  - unit tests next to `pinned_env_keeps_host_config_rustflags_out_of_a_bootstrapped_cargo`
    (841): `cargo.sysroot` is set to the detected sysroot; an existing key is
    kept; the hint is logged for a sysroot without `library/Cargo.toml`.
- `plugins/rust/plugin.toml` 133-170 (comments of
  `[plugin.semantic.initialization_options]`): note that `cargo.sysroot` is
  added at run time and why.
- `core/tests/plugin_memory_limit.rs` ~327-360 (GM-584 no-std variant sets
  `cargo.sysrootSrc`): check that it still yields a no-std tree with
  `cargo.sysroot` set (`RustLibSource::Path` + src override is honoured by
  RA, `workspace.rs` 240); no change expected.
- `plugins/rust/README.md`: one line under the semantic tier: std needs
  `rust-src` for the pinned toolchain; the plugin never installs it.
- `docs/adr/0033-...`: optional note that RA's rust-src auto-install is
  closed by pinning `cargo.sysroot`.

## How verify confirms std loads on the x86_64 runner

1. Locally (x86_64 macOS): the existing GM-584 std test
   (`a_real_rust_analyzer_over_its_memory_limit_is_suspended_and_structural_work_continues`)
   passes; with a throwaway `RUSTUP_HOME` toolchain that has no rust-src,
   the plugin logs the B2 hint and **no** `rustup component add` happens
   (no `downloads/` activity, `rustup component list --installed` unchanged).
2. Control for B: remove the `cargo.sysroot` insert -> with the no-rust-src
   toolchain, rust-src appears after the run (RA installed it). Control for
   A is CI itself (step 3).
3. CI (needs a push, owner's call): on the branch run, the
   `tests (x86_64-apple-darwin)` job's "rust-src state" step prints `rust-src` in the
   installed list before any test could add it, a full library
   (~3483 files, 30 vendor crates) and `sysroot cargo metadata exit: 0`;
   the memory-limit std test passes. A dispatch of the workflow 3 times on
   x86_64 macOS gives 3/3.
