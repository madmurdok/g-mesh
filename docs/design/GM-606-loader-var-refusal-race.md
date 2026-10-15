# GM-606: the cargo `[env]` loader-var refusal missed once under load

Status: diagnosis (S1). Branch `fix/GM-606-loader-var-refusal-race`, off
release-4.9.0 (83482ac).

## Failure

Batch-end full suite of 4.9.0 (`scripts/test-local.sh --full`, nextest,
load 20-160), 2026-10-15 02:02:

```
FAIL [13.527s] g-mesh-plugin-rust::semantic_runs_no_project_code
  cargo_env_loader_var::a_cargo_config_env_loader_var_refuses_the_semantic_tier_and_fires_no_marker
test result: FAILED. 0 passed; 1 failed; ...; 4 filtered out
[rust] semantic tier: .../rust-analyzer (rust-analyzer 1.97.1 ...)
[rust] semantic pass: 2 file(s), 2 node(s)/1 edge(s) upserted, ... in 9.47s
panicked at plugins/rust/tests/semantic_runs_no_project_code.rs:791:9:
the semantic tier started a rustc that loaded the project's [env] DYLD_INSERT_LIBRARIES dylib
```

What the log alone proves:

- `4 filtered out`: nextest ran this test alone in its own process. No
  other test of the binary shared its environment or cwd.
- The `[rust] semantic tier:` line is logged by `semantic::engine`
  (`plugins/rust/src/semantic.rs:118`) *after* the
  `cargo_env_loader_var(root)` check (l.112). So in that process the check
  returned `None` for a root whose `.cargo/config.toml` had, moments
  earlier, made `cargo metadata` load the dylib (the test's sanity
  assertion at l.773 passed).

## The refusal code path

`semantic::engine` (`plugins/rust/src/semantic.rs:101`) calls
`cargo_env_loader_var(root)` (l.260) once, before it resolves the server.
Its only callers are `engine` and the unit-test child
`semantic::tests::cargo_env_loader_var_child` (g-mesh `find_callers`). The
check is a pure filesystem walk: no subprocess, no timeout, no `cargo config
get`. It reads, in cargo's order, `.cargo/config.toml` and `.cargo/config`
in `root` and every ancestor, then in each immediate subdirectory, then
`$CARGO_HOME`, following `include`s.

Every way it can return `None` while a hostile file exists is an error
turned into "allow":

| Site | Behaviour on error |
|---|---|
| l.262 `read_dir(root)` | `Err` skips the subdirectory configs |
| l.285 `!file.is_file()` | `Path::is_file` is `false` on *any* `stat` error, not only `ENOENT`; also `false` for a non-regular file cargo would still read |
| l.288 `read_to_string` | any `Err` (`EMFILE`, `ENFILE`, `EIO`, ...) `continue`s; the doc comment's "cargo cannot read it either" holds for `EACCES`, not for a transient error |
| l.329 `cargo_home()` | `None` without `HOME`: the home config is skipped |

## Hypotheses

**(a) Another test mutates the environment or cwd.** Ruled out for this
failure. nextest ran the test alone in its process (`4 filtered out`), and
the check does not depend on env or cwd for the root config anyway: the
fixture's `root/.cargo/config.toml` is the first file popped (absolute path
from the canonical scratch root). `HOME`/`CARGO_HOME` only change the last
file read. Under plain `cargo test` the five tests of this binary share one
process and mutate `PATH`, cwd and `CARGO*` concurrently (the file header
still says "One test in this binary", it has five; GM-602), but the
`whole`-binary loop below ran that mode 15 times without a miss.

**(b) A load-sensitive path (timeout, subprocess, caught error).** No
timeout or subprocess exists in the check. The caught-error-to-allow
sites in the table above are the only load-sensitive path.

**(c) Config written after the engine starts.** Not in this test: the
config is written, then `cargo metadata` in the fixture loads the dylib
(sanity at l.773 passed), then `engine` runs. But (c) *is* a production
gap in a different shape: see "Production impact" (confirmed).

**(d) A stale or control-patched binary ran.** Ruled out. In
`g-mesh-wt-rel490` the sources `plugins/rust/src/semantic.rs` and the test
file have mtime 10-14 15:07:32 (the GM-587 merge); the suite's test
binaries were built 10-15 01:39; reflog shows no checkout after 01:37 (the
GM-580 merge). An edit-and-revert would have moved the mtime.

**(e) Scratch-dir collision.** The fixture dir is
`$TMPDIR/g-mesh-rust-gm513-loader-env-<pid>-<n>`; only this test uses that
name and two live processes cannot share a pid. No test sweeps `$TMPDIR` by
prefix (grep of `core`, `plugins`, `scripts`). Ruled out.

## Repro attempt

Temporary `eprintln` traces in `engine` and `cargo_env_loader_var` (every
candidate file's `metadata`, `read_to_string` result, parsed `[env]` keys,
and the decision) - patch kept at `scratchpad/gm606/trace.patch`, reverted.
One fail-fast background script: 32 x `yes`, 8 workers each looping the
test alone (`--exact`, a fresh process per run, as nextest does), 3 workers
looping the whole binary (all five tests in one process, rust-analyzers
included).

| | runs | pass |
|---|---|---|
| single-test workers | 4,519 | 4,519 |
| whole-binary workers | 15 | 15 |

Load averages 20 at start, 541 after 5 min, 795 at 30 min, 880 (5-min avg)
at the end; 02:15-04:49 (the script's deadline was checked between
iterations, so the whole-binary runs overshot it). Every trace showed
`root/.cargo/config.toml` `meta=Ok((true, 171))`, `read Ok(171)`,
`env_keys=["DYLD_INSERT_LIBRARIES"]`, `found=Some(...)`. A single run
unloaded: `real 4.01 user 0.38 sys 0.43`.

What differs in the full suite and was not reproduced: 130 other binaries
in parallel (TypeScript/pyright servers, daemons), other agents' suites on
the same machine, a shared `$TMPDIR`. None of these touches the fixture dir
or the check's inputs except through the system-wide file table (`ENFILE`;
`kern.maxfiles` 245760, `kern.num_files` 9316 when measured afterwards,
idle) or an I/O error. GM-584's sysroot failure and offline cargo act on
rust-analyzer after the check, so they cannot flip it.

## Cause

**Not reproduced.** The log proves the check returned `None` in a process
whose fixture held the hostile config and had just fired the sanity
marker. With (a), (d), (e) ruled out and no timeout or subprocess in the
check, the only code path that yields `None` there is a filesystem error
turned into "allow" at l.285 or l.288 (most likely a transient `stat`/`open`
failure under the suite's load; which errno is unknown). What would settle
it: the errno of that `stat`/`read`. The fail-closed fix below makes the
next occurrence a refusal whose message names the file and the error,
instead of a marker.

## Production impact

1. **The flake's path is production code.** `engine` is the production
   factory (`LazyEngine` calls it at the first semantic pass). A transient
   `stat`/`read` error on the hostile config starts rust-analyzer, whose
   `cargo metadata` reads the same file a moment later and loads the dylib.
   Probability: unknown, at most 1 in the full suite against 4,534 clean
   runs here. It is a security miss, not only a test race.

2. **Confirmed, deterministic: a config that appears after the engine
   started is never checked.** The check runs once per plugin process.
   rust-analyzer watches the workspace itself (`files.watcher` is "client",
   which falls back to its own notify watcher since the bridge registers no
   watched files) and reruns `cargo metadata` on a `Cargo.toml`/`Cargo.lock`
   change (`cargo.autoreload`, default `true`). Temporary probe test
   (`scratchpad/gm606/toctou-probe.patch`, reverted; the same fixture and
   dylib): engine started on a clean fixture, first pass ran, mark absent;
   then `.cargo/config.toml` with the loader var plus an edit of
   `app/Cargo.toml` -> **mark present after 9.5 s** (`real 33.74 user 10.59
   sys 6.00`, load 49). The config alone, with no manifest edit, did not
   fire within 90 s (`real 111.63`). In the field: a `git pull`/`checkout`
   that adds `.cargo/config.toml` and touches a manifest or lockfile while
   the daemon runs. The `workspaceChanged` g-mesh sends does not re-run the
   check (`LazyEngine::workspace_changed`, sdk `semantic.rs:351`, called from
   `run::Session::handle` and `Session::resolution_changed`; it only
   `unsettle`s the client, `bridge.rs:2818`), and rust-analyzer reloads on
   its own watcher before that message would arrive anyway.

## Fix proposal

**F1 - fail closed (fixes the flake's path).** `cargo_env_loader_var`
returns a verdict that distinguishes "could not check":

- l.284-288: replace `file.is_file()` with `std::fs::metadata(&file)`;
  `NotFound` (and `NotADirectory`) -> skip; `Ok` regular file -> read; `Ok`
  non-regular file or any other `Err` -> refuse, naming the file and the
  error. `read_to_string`: `NotFound` (deleted between stat and open) ->
  skip; any other `Err` -> refuse.
- l.262 `read_dir(root)`: `Err` other than `NotFound` -> refuse.
- l.112 `engine`: bail on both "found" and "could not check", with
  distinct messages ("... sets {var} ..." vs "could not read {file}: {err};
  refused rather than unchecked").
- Cost: a project with an unreadable `.cargo/config.toml` (`EACCES`) loses
  its semantic tier; cargo itself fails on such a file, so rust-analyzer
  would not have a project model there either.

**F2 - re-check before every reload (closes the confirmed gap).**

- `plugins/rust/plugin.toml:162` `[plugin.semantic.initialization_options]`:
  add `cargo.autoreload = false` so rust-analyzer never reruns
  `cargo metadata` on its own.
- New rust engine wrapper in `plugins/rust/src/semantic.rs` (returned by
  `engine` at l.135 instead of the bare `LspBridge`): implements
  `SemanticEngine`; `workspace_changed` and the start of each `answer`
  re-run the check; hostile or unreadable -> stop the server (bridge
  shutdown / `kill_live_servers` path) and answer `Err` from then on, which
  `LazyEngine` already turns into structural-only; clean ->
  `bridge.workspace_changed()` plus a `rust-analyzer/reloadWorkspace`
  request.
- SDK: `LspBridge` needs a way to send that request (a small
  `pub fn request(&mut self, method, params)` or a manifest key
  `reload_request` used by `workspace_changed`, `plugins/sdk/src/lsp/bridge.rs:2818`).
- Residual: with autoreload off, a `Cargo.toml` change reaches
  rust-analyzer only through g-mesh's `workspaceChanged`; the
  `workspace_changed` test (`plugins/rust/tests/workspace_changed.rs`) and
  the conformance suite must still pass. A config added with no manifest
  change and no `workspaceChanged` is caught at the next pass's re-check.
- The OS sandbox (GM-587 D2) remains the structural fix for both.

**F3 - test hygiene (only the test).** The test's header claim "One test
in this binary" is false (5 tests); GM-602 owns moving them to
child-process envs as GM-588 did for Python. Not the cause here.

## Tests and controls

- Unit (`semantic.rs` tests, child-process style as the existing
  `loader_child`): a config path that `stat`s with an error other than
  `NotFound` (a `.cargo/config.toml` that is a directory, or a dangling
  include target's parent made unreadable) -> refusal naming the file.
  Control: restore `!file.is_file() ... continue` -> the test fails.
- Unit: a `read_to_string` failure (file mode 000, run only when not root)
  -> refusal. Control: restore `let Ok(text) = ... else { continue }`.
- Integration (`semantic_runs_no_project_code.rs`, from
  `toctou-probe.patch`, asserting the opposite): engine starts on a clean
  fixture, first pass runs, then config + `app/Cargo.toml` edit +
  `workspace_changed` -> the next `answer` is `Err` naming the config, and
  no marker within 30 s. Control 1: drop `cargo.autoreload = false` from
  the manifest -> marker fires (9.5 s measured). Control 2: drop the
  re-check in `workspace_changed`/`answer` -> marker fires or answer is
  `Ok`.
- Repeat the new integration test 5 times in the tests slice and 5 times
  in verify (once under load), per the standing rules.

## Owner decisions

- **D1 - fail closed on a config the check cannot read (recommended).**
  Benefit: closes the flake's only code path; the next occurrence names the
  errno. Risk: an unreadable config or a transient error downgrades a
  project to structural for the plugin process's life (until restart).
- **D2 - close the after-start gap now with F2 (recommended), or file it
  as its own task behind the sandbox (GM-587 D2).** Benefit: the confirmed,
  deterministic miss is closed in this release. Risk: turning off
  `cargo.autoreload` changes how every Rust project picks up manifest
  edits; it touches the SDK bridge (new request API) and needs the
  workspace-changed and conformance tests to hold.
- **D3 - the flaky test itself.** Keep it as is (with F1, a recurrence
  fails as a refusal with a reason, not a marker), or mark it for retry in
  `.config/nextest.toml`. Recommended: keep, no retry - a retry would hide
  a security miss.

## g-mesh calls used

- `select_project g-mesh-wt-gm606`.
- `find_definition cargo_env_loader_var` -> `plugins/rust/src/semantic.rs:260`
  (and the test module of the same name).
- `find_callers` on it -> `semantic::engine`,
  `semantic::tests::cargo_env_loader_var_child`.
- `find_callers LazyEngine::workspace_changed` -> `run::Session::handle`,
  `run::Session::resolution_changed`, one sdk unit test (plus 2 untyped
  receiver calls in `plugins/sdk/tests/lsp_bridge.rs`).
