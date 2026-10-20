# GM-600: run rust-analyzer under a macOS Seatbelt sandbox

Status: design (slice S1), awaiting owner decisions D1-D4.
Branch: `feat/GM-600-macos-seatbelt-sandbox` (off `release-4.14.0`).
Follows: GM-587 D2 ("Split per OS"); Linux counterpart GM-601 targets the
launch abstraction in section 4.

## Question

GM-587 ships a refuse-to-structural pre-check (`cargo_env_loader_var`,
ADR 0037) for one channel: a cargo config `[env]` dynamic-loader variable.
The structural fix it recommended is an OS sandbox around the whole
rust-analyzer process tree that denies mapping executable code from, and
exec of, anything outside the toolchain and the OS. This note decides how
the Rust plugin starts rust-analyzer under Seatbelt on macOS, what the
profile allows, what happens when the sandbox cannot be applied, the
launch abstraction GM-601 reuses, and how the tests pin it.

## Experiments (this Mac, 2026-10-20)

Host: macOS 26.6.2 (25G83), x86_64, toolchain `stable-x86_64-apple-darwin`
(rustc 1.97.1). `uptime`: load average 24.9 / 23.1 / 12.8, 14 days up, so
timings are indicative only; every LSP arm was gated on rust-analyzer's
`quiescent` status, not a timer. `/usr/bin/time -p` of the LSP arms: `real`
20-41 s at `user` 0.3-0.4 s for the driver (the driver waits on the server;
the server's own CPU is not in that figure).

Scratch: `scratchpad/gm600/` of this session (`ra.sb`, `lsp_drive.py`,
`arms.sh`, `arms-summary.txt`, `fx/`). The GM-587 scratch no longer holds
its R9 profile (only `fx/` sources survive), so the R9 shape was rebuilt
from the GM-587 note's description.

Fixture `fx`: workspace `app` (`main.rs` calls `util::Thing.method()`),
a marker dylib `fx/dylib/marker.dylib` whose constructor appends
`pid ppid` to `fx/MARK`, and `fx/.cargo/config.toml`
`[env] DYLD_INSERT_LIBRARIES = { value = "<fx>/dylib/marker.dylib", force = true }`.
LSP arms use `pinned_env`'s exact variables and the plugin's init options
(`checkOnSave=false`, build scripts and proc macros off, `cargo.sysroot`
pinned), wait for `quiescent`, then ask `definition` of `method`.

### `sandbox-exec` behaviour

| # | Probe | Result |
|---|---|---|
| E1 | `sandbox-exec -p '(version 1)(allow default)' /usr/bin/true` | rc 0. Present and working on macOS 26.6.2; man page says "DEPRECATED" (unchanged since 2017). |
| E2 | profile syntax error / unknown operation / missing `-f` file | rc **65**, message on stderr (`syntax error`, `unbound variable`, `No such file or directory`). |
| E2c | `(param "MISSING")` not passed with `-D` | rc **65** (`invalid data type of path filter`). A forgotten parameter is a profile error, not a silent allow. |
| E3 | target missing / target denied `process-exec` | rc **71**, `execvp() of '<path>' failed: <errno>`. |
| E4 | `sandbox-exec` inside `sandbox-exec` | rc 0: nesting works (it also worked from this agent's own shell). |
| E5 | `--` before the command; unused `-D` | both accepted. |
| E6 | `sandbox-exec` replaces itself (`execvp`) | the spawned pid *is* rust-analyzer: the SDK's kill/wait of the child needs no change. |
| E7 | deny on `/tmp/x` alias vs real `/private/tmp/x` | a deny written on the alias did **not** block exec through the alias (rc 0); written on the canonical path it did. Seatbelt matches resolved paths: every path parameter must be canonicalized. |
| E8 | `(allow … (subpath "/usr"))` then `(deny … (subpath "/usr/bin"))` | the later, narrower deny won (rc 71). Reverse checks: `(deny /usr)(allow /usr/bin)` runs `/usr/bin/true` (rc 0); `(allow /usr/bin)(deny /usr)` refuses it (rc 71). **Last matching rule wins, whatever its specificity**, so the generator must order rules itself. |

### The allowlist profile (R9 shape) against the fixture

Profile `ra.sb` (default allow; default-deny for `file-map-executable` and
`process-exec`; allow the OS, the sysroot and `~/.cargo/bin`; deny the
project root last):

| Arm | Marker | rust-analyzer | `definition` of `method` |
|---|---|---|---|
| U0 hostile, unsandboxed | **29 lines** (every `rustc`/`rustup` child) | health ok | resolved to `util.rs` |
| S1 hostile, sandboxed | **0** | health **warning**: "Failed to read Cargo metadata with dependencies"; stderr shows `dyld: terminating because inserted dylib … could not be loaded … (file system sandbox blocked mmap())`, `rustc -vV` SIGABRT | **resolved** to `util.rs` |
| S2 clean, sandboxed | 0 | health ok | resolved |
| U2 clean, unsandboxed | 0 | health ok | resolved |
| B/C `cargo metadata` sandboxed, toolchain cargo and rustup proxy | 0 | rc 101: `rustc -vV` SIGABRT | - |
| D exec of a project binary | - | rc 71, `Operation not permitted` | - |

Two load-bearing facts:

1. **On current macOS dyld aborts a process whose inserted dylib is
   blocked** (it does not skip it). Under the sandbox the hostile config
   therefore makes every `rustc` cargo starts die, so `cargo metadata`
   *with dependencies* fails. rust-analyzer still falls back and serves the
   workspace (S1 resolved the cross-module call), so the semantic tier is
   reached with a warning; dependency crates would be unresolved. This
   fallback is rust-analyzer behaviour, not ours; section 5 pins it so an
   RA upgrade that drops it is noticed.
2. The sandbox costs nothing observable on a clean project (S2 vs U2,
   both resolved, timings within load noise).

## 1. Launching rust-analyzer under `sandbox-exec`

`engine()` (after `resolve` and `pinned_env`, before `LspBridge::new`)
rewrites the resolved `SemanticConfig`:

```
command = /usr/bin/sandbox-exec          (absolute: SIP-protected, never a PATH lookup)
args    = ["-p", <profile text>, "-D", "ROOT=<canon root>", "-D", "ALLOW_0=<canon prefix>", …,
           "--", <resolved rust-analyzer>, <original args…>]
env     = unchanged (sandbox-exec passes it on; it is not itself affected by DYLD_*)
```

- The profile is a fixed text compiled into the plugin; the only variable
  parts are paths, passed as `-D` parameters (no string escaping of user
  paths into SBPL, no temp file). `-p` keeps it off disk.
- **Availability check / preflight, once per `engine()`:** run the wrapped
  command with `--version` through the SDK's public `lsp::probe`
  (`plugins/sdk/src/lsp/resolve.rs:148`) under `PROBE_BUDGET`. It proves in
  one exec that `sandbox-exec` exists, the profile parses with every
  parameter bound (E2/E2c), the server binary may be exec'd (E3) and its
  own libraries may be mapped. Outcome mapping:
  - `/usr/bin/sandbox-exec` missing (spawn `ENOENT`) -> `Failed("sandbox-exec not found")`.
  - exit 65 -> `Failed("Seatbelt profile rejected: <stderr first line>")` (a plugin bug or a macOS that changed SBPL).
  - exit 71 -> `Failed("Seatbelt denied starting rust-analyzer: <stderr>")` (allowlist misses the server's prefix).
  - other non-zero / timeout / output without a version -> `Failed(<detail>)`.
  - success -> `Confined { backend: "seatbelt" }`, logged once with the allowed prefixes.
- **Deprecated-path behaviour:** `sandbox-exec` is deprecated since 10.x
  but present and fully functional on 26.6.2 (E1-E8); it is what Chromium,
  Bazel and several agent sandboxes still use. If Apple removes it, the
  preflight reports `sandbox-exec not found` and the degrade policy (D1)
  applies; no silent loss.
- `resolve()`'s own `rust-analyzer --version` probe stays unsandboxed: it
  runs the toolchain's binary with no project in scope and starts no child.
- Error messages from the bridge that print `config.command`
  (`bridge.rs:735`, `:744`) will name `/usr/bin/sandbox-exec`. Accepted;
  the start-up log line names the confined server.

## 2. The allowlist profile

```scheme
(version 1)
(allow default)
(deny file-map-executable)
(deny process-exec)
;; emitted in ascending path-length order, so the most specific rule wins (E8):
(allow file-map-executable process-exec (subpath (param "ALLOW_n")))   ; per toolchain prefix
(deny  file-map-executable process-exec (subpath (param "ROOT")))
;; fixed OS prefixes:
(allow file-map-executable (subpath "/System") (subpath "/usr/lib") (subpath "/Library/Apple"))
(allow process-exec (subpath "/bin") (subpath "/usr/bin") (subpath "/usr/libexec") (subpath "/System"))
```

Scope is exactly the class GM-587 proved: mapping code and exec. File
reads, writes and network stay as today (`allow default`); widening is D4.

Paths derived at runtime, all canonicalized (E7), deduplicated, and each
skipped when `None`:

| Prefix | Source | Why |
|---|---|---|
| toolchain sysroot | `Toolchain.sysroot` (`rustc --print sysroot`, already computed by `Toolchain::detect`) | `rustc`, `cargo`, `rust-analyzer`, `librustc_driver`, `libexec/rust-analyzer-proc-macro-srv` all live here; on rustup this is `$RUSTUP_HOME/toolchains/<pin>` |
| rustup proxies dir | parent of `Toolchain.rustup` (canonical `rustup`, i.e. `$CARGO_HOME/bin`) | rust-analyzer runs `rustup which cargo` and cargo may run proxies (GM-587 R1); sccache, if ever re-enabled as a wrapper, lives here too |
| server dir | parent of the canonical resolved `rust-analyzer` | non-rustup installs (Homebrew formula) keep it outside the sysroot |
| `rustc` / `cargo` dirs | parents of `Toolchain.rustc` / `.cargo` | non-rustup: binaries on `PATH`, outside any rustup home |
| `ROOT` (deny) | canonical project `root` | never map or exec anything the project ships, even when the project lies inside an allowed prefix (e.g. indexing `library/` inside the sysroot) |

`$RUSTUP_HOME` and `$CARGO_HOME` need no lookup of their own: the sysroot
and the canonical `rustup` path already point inside them. A custom
`CARGO_TARGET_DIR`/`[build] target-dir` outside `ROOT` is not allowed by
default-deny, so no target-dir parameter is needed.

sccache: `pinned_env` empties all four wrapper variables, so sccache never
runs inside the rust-analyzer tree; the test builds' sccache runs outside
the sandbox and is unaffected. The proc-macro server is in the sysroot
and allowed; a project proc-macro dylib (in `target/`, under `ROOT`) is
denied, which matches `procMacro.enable = false` today.

Ordering rule: if the project root is an ancestor of an allowed prefix
(indexing `$HOME`, which contains `~/.rustup`), the narrower allow is
emitted after the deny and wins, so the toolchain still works; if the root
is inside an allowed prefix, the root deny is narrower and wins.

## 3. Degrade policy when the sandbox is missing or fails

Recommended (D1, option A): **on macOS, a failed preflight refuses the
semantic tier** - `engine()` returns `Err(reason)`, `LazyEngine` logs it and
the plugin stays structural for the process, exactly like a GM-587
refusal. Unsandboxed rust-analyzer is never started on macOS.
On an OS with no backend yet (Windows; Linux until GM-601 lands), the
outcome is `NoBackend`: log once and start unsandboxed, today's behaviour
plus the GM-587 pre-check. Benefit: the guarantee "on macOS, indexing never
maps project code" holds unconditionally. Risk: an allowlist miss on an
unusual install (Homebrew dylib deps, see D3) costs that user the semantic
tier until fixed; the logged reason names the denied path's prefix.

## 4. Launch abstraction for GM-601

New module `plugins/rust/src/sandbox.rs` (the Rust plugin only; the SDK
does not change - `SemanticConfig.command/args` already carry a wrapper,
and every other plugin keeps its unsandboxed start):

```rust
/// What the rust-analyzer process tree may map executable and exec,
/// independent of the OS. All paths canonical.
pub struct SandboxPolicy {
    /// Nothing under it is mapped executable or exec'd.
    pub project_root: PathBuf,
    /// Toolchain prefixes the tree may map and exec (sysroot, rustup
    /// proxies dir, server dir, rustc/cargo dirs). OS prefixes are each
    /// backend's own.
    pub toolchain: Vec<PathBuf>,
}

pub enum SandboxOutcome {
    /// `config` now starts the server, and every child, confined.
    Confined { backend: &'static str },
    /// No backend on this OS; `config` untouched.
    NoBackend,
    /// A backend exists but could not confine here; `config` untouched.
    Failed(String),
}

/// Rewrites `config` (command, args, env) so the server starts under this
/// OS's sandbox, after a preflight of the wrapped command.
pub fn confine(config: &mut SemanticConfig, policy: &SandboxPolicy) -> SandboxOutcome;

impl SandboxPolicy {
    /// Derived from the detected toolchain and the resolved server.
    pub(crate) fn for_server(root: &Path, toolchain: &Toolchain, server: &Path) -> Self;
}
```

`confine` dispatches by `#[cfg(target_os = "macos")] mod seatbelt;`
(this task) and `#[cfg(target_os = "linux")] mod linux;` (GM-601); other
targets return `NoBackend`. GM-601 implements
`linux::confine(&mut SemanticConfig, &SandboxPolicy) -> SandboxOutcome`
(Landlock in a re-exec'd helper or a `bwrap` argv prefix) and adds its
system prefixes inside its own module. `engine()` is the only caller and
owns the policy: `Confined` -> log; `NoBackend` -> log and continue;
`Failed(r)` -> `bail!` (D1). `SandboxPolicy::for_server` and `confine` are
`pub` so the test in section 5 can build a confined bridge without the
GM-587 pre-check.

## 5. Fixture and how the test disables the pre-check

Extend `plugins/rust/tests/semantic_runs_no_project_code.rs`, module
`cargo_env_loader_var` (reuse `loader_fixture_in_its_env`,
`mark_and_dylib`, `build_marker_dylib`, the `in_its_env` parent/child
harness). New test pair, macOS only
(`#[cfg(target_os = "macos")]`):
`a_sandboxed_server_maps_no_project_dylib_and_still_resolves` / `..._in_its_env`.

- Fixture: the existing loader fixture (workspace `app`, marker dylib in
  `<root>/dylib`, `.cargo/config.toml` `[env] DYLD_INSERT_LIBRARIES = { value, force = true }`).
  Keep the existing sanity step: an unpinned `cargo metadata` in the
  fixture writes the marker (proves the hostile config is live).
- **Disabling the pre-check:** the test does not call `semantic::engine`
  (which refuses). It builds the server the way `engine` does minus the
  refusal, as `project_extra_env::index_fixture` already does for its
  client arm: `SemanticConfig::from_manifest()`, `command` = the real
  rust-analyzer, the pinned variables, then
  `semantic::sandbox::confine(&mut config, &SandboxPolicy::for_server(...))`
  and `LspBridge::new("rust", &root, config).quiescent_signal()`. To avoid
  a second copy of `engine`'s setup, split `engine` into
  `pub fn server_config(root) -> Result<SemanticConfig>` (manifest,
  resolve, pinned env, sysroot, **confine**) and `engine` = pre-check +
  `server_config` + bridge; the test calls `server_config`. No env var or
  feature flag turns the pre-check off in production code.
- Assertions: `confine` returned `Confined { backend: "seatbelt" }`;
  after `answer`, `MARK` does not exist; `resolved_method(&index, &answer)`
  is true (the semantic tier was reached, S1's fallback).
- Second test (unit, `--lib`, macOS): `seatbelt` profile text +
  args for a given policy: parameters bound for every prefix, `ROOT`
  deny present, rules in ascending length order; and a preflight against
  a policy that omits the server's dir returns `Failed` (exit 71).

## 6. Edit map

Line numbers 1-based, from `get_file_outline` (0-based) + 1.

Change:
- `plugins/rust/src/semantic.rs` `engine` (105-149): split out
  `server_config` (manifest -> resolve -> log -> `pinned_env` -> sysroot pin
  -> `sandbox::confine`); `engine` keeps `cargo_env_loader_var` (122-124)
  before it and `LspBridge::new` (147) after it. On
  `SandboxOutcome::Failed` -> `bail!`, on `NoBackend` -> log. Doc comment:
  add the invariant (present tense) and link the new ADR.
- `plugins/rust/src/sandbox.rs` (new): `SandboxPolicy`,
  `SandboxOutcome`, `confine`, `for_server`; `mod seatbelt` (profile text,
  argv builder, preflight via `lsp::probe`, exit-code mapping).
- `plugins/rust/src/lib.rs` (21): `pub mod sandbox;`.
- `plugins/rust/src/semantic.rs` `pinned_env` doc (464-487): the `[env]`
  sentence gains "and, on macOS, the server runs under Seatbelt (see
  `sandbox`)".
- `plugins/rust/tests/semantic_runs_no_project_code.rs` module
  `cargo_env_loader_var` (888-1197): new test pair (section 5).
- `docs/adr/0039-rust-server-runs-under-an-os-sandbox.md` (new, next free
  number after 0038): records D1-D4; ADR 0037 gains a "superseded in part
  on macOS" pointer.

Read for context (no change):
- `Toolchain` / `Toolchain::detect` (`semantic.rs` 227-243, 246-272): the
  prefixes' sources.
- `resolve` (`semantic.rs` 725-742) and `lsp::probe`
  (`plugins/sdk/src/lsp/resolve.rs` 148-): preflight reuse.
- `LspClient::start` (`plugins/sdk/src/lsp/client.rs` 225-300): spawns
  `config.command` with `args`, `envs`, `current_dir(root)`; unchanged.
- `CheckedBridge::checked` (`semantic.rs` 179-188): the per-pass GM-587
  re-check stays as is (defense in depth).
- `project_extra_env::index_fixture` (`tests/semantic_runs_no_project_code.rs`
  750-783): the bypass-`engine` pattern the new test copies.

## 7. Behaviours to pin and controls

Behaviours (one control each where marked):

- B1 On macOS the server starts under Seatbelt: the hostile fixture fires
  no marker. (Ctl-1)
- B2 The confined server still reaches the semantic tier on the hostile
  fixture (resolves `method`). Guarded by the sanity assertion; a
  control would be "break RA's fallback", not ours - no control.
- B3 The project root is denied even inside an allowed prefix (ordering).
  (Ctl-3)
- B4 A failed preflight refuses the semantic tier (D1). (Ctl-4)
- B5 The toolchain allowlist is load-bearing: the clean-fixture tests that
  already resolve through the confined server keep passing. (Ctl-5)
- B6 Profile parameters are canonical paths (E7). (Ctl-6)

Controls (6, the verify slice reverts code in a throwaway worktree;
group: 1-2 in the integration binary, 3-6 in `--lib`):

1. **Ctl-1 profile disabled:** make `seatbelt::confine` return
   `Confined` without rewriting `config`. The new integration test must
   fail with the marker written. (The AC's "profile disabled -> marker
   fires".)
2. **Ctl-2 map rule dropped:** remove `(deny file-map-executable)` from
   the profile text. Same integration test must fail (marker), proving
   the map denial, not the exec denial, closes the `[env]` channel.
3. **Ctl-3 ordering:** emit rules in insertion order instead of ascending
   length. The `--lib` ordering test (root inside an allowed prefix) must
   fail.
4. **Ctl-4 degrade:** treat `Failed` like `NoBackend` in `engine`. The
   `--lib` test that runs `engine`'s outcome handling with a failing
   preflight must fail (it expects `Err`).
5. **Ctl-5 allowlist:** drop the sysroot from `SandboxPolicy::for_server`.
   The `--lib` preflight test for a real toolchain must fail (`Failed`,
   exit 71).
6. **Ctl-6 canonical:** skip `canonicalize` on `project_root`. The `--lib`
   test with a root given through a symlink (`/tmp` alias) must fail.

## 8. Owner decisions

Answered 2026-10-20 (owner's chosen labels quoted):

- D1: "Structural с причиной (Recommended)" - option A.
- D2: "Локальные вызовы + нет маркера (Recommended)" - option A.
- D3: "Разрешить Homebrew/nix" - option B: the profile also allows
  `/opt/homebrew`, `/usr/local`, `/opt/local` and `/nix/store`; the
  project-root deny (emitted after, by ascending length) still covers a
  project inside one of them.
- D4: "Только загрузка/запуск, сеть — отдельной задачей (Recommended)" -
  option A; network/write denial is a follow-up task.

### D1 - when the sandbox cannot be applied on macOS (recommended: A)

- **Today:** rust-analyzer always starts unsandboxed; GM-587's pre-check
  refuses only the `[env]` loader-var channel.
- **Change:** the plugin now tries to start it under Seatbelt; the
  question is what to do when that fails (no `sandbox-exec`, profile
  rejected, allowlist misses the server).
- **Example:** a future macOS removes `/usr/bin/sandbox-exec`; or a
  Homebrew rust-analyzer's dylib lives under `/usr/local/opt/...`, outside
  the allowlist, and the preflight exits 71.
- **A. Refuse to the structural tier with the logged reason (recommended).**
  Benefit: "on macOS, indexing never maps or execs project code" holds
  without exceptions. Risk: such users lose cross-file semantic edges until
  the allowlist is fixed or they report it.
- **B. Start unsandboxed with a warning.** Benefit: no user ever loses
  the semantic tier. Risk: the protection silently disappears on exactly
  the machines where it failed; GM-587's pre-check is then the only guard,
  and AC3 channels 2-5 are open again there.
- **C. A, plus an explicit opt-out env var for the daemon (`G_MESH_RUST_SANDBOX=off`).**
  Benefit: a stuck user has a way out. Risk: one more knob; a daemon-env
  var cannot be set by a project, but it can be left set and forgotten.

### D2 - what "still reaches the semantic tier" means for a hostile project (recommended: A)

- **Today:** the AC says the hostile fixture "still reaches the semantic
  tier".
- **Change / finding:** on macOS 26 dyld *aborts* a process whose inserted
  dylib is blocked (S1). Under the sandbox every `rustc` cargo starts dies,
  `cargo metadata` with dependencies fails, and rust-analyzer falls back
  (health "warning") but still resolves workspace-local calls.
- **Example:** the fixture's `util::Thing.method()` resolves; a call into
  a dependency crate in the same project would stay unresolved.
- **A. Pin "workspace-local calls resolve, no marker" (recommended).**
  Benefit: matches what Seatbelt can deliver; the test notices if an RA
  upgrade drops the fallback. Risk: the test depends on RA's fallback
  behaviour, which we do not control.
- **B. Pin only "no marker, engine started".** Benefit: independent of RA
  internals. Risk: a sandbox that broke indexing outright would pass.
  (In production nothing changes either way: the GM-587 pre-check still
  refuses such a project before the server starts.)

### D3 - package-manager prefixes in the allowlist (recommended: A)

- **Today:** no allowlist exists.
- **Change:** the profile allows the OS, the sysroot, the rustup proxies
  dir and the server's own dir. Toolchains not installed by rustup may
  link libraries elsewhere (Homebrew `cargo` links `libgit2`/`openssl`
  under `/usr/local/opt` or `/opt/homebrew/opt`).
- **Example:** a user with `brew install rust rust-analyzer` and no rustup:
  the preflight passes (the server starts) but `cargo metadata` aborts on
  a denied `libgit2` map, so dependencies stay unresolved, like S1.
- **A. Derived prefixes only; non-rustup installs that break are fixed when
  reported (recommended).** Benefit: the tightest profile; nothing a user
  can write to by default is allowed. Risk: Homebrew-only users may degrade
  (D1) or index with a warning.
- **B. Also allow `/opt/homebrew`, `/usr/local`, `/opt/local`, `/nix/store`.**
  Benefit: package-manager toolchains work out of the box. Risk: these
  trees are user-writable (Homebrew), so a dylib dropped there by other
  means would be mappable; the project root deny still covers a project
  that lives inside them.
- **C. Derive library dirs from `otool -L` of the toolchain binaries.**
  Benefit: exact. Risk: more code, a subprocess per start, transitive deps
  to follow.

### D4 - profile scope beyond map/exec (recommended: A)

- **Today:** ADR 0033 says the tier writes nothing and reaches no network;
  that is enforced by configuration (`CARGO_NET_OFFLINE`, init options),
  not by the OS.
- **Change:** Seatbelt could also `(deny network-outbound)` and deny
  writes outside the cargo/RA caches and `TMPDIR`.
- **Example:** an unknown future channel runs code in-process; with map
  and exec denied it cannot load a payload, but plain computation inside an
  allowed binary could still write a file or open a socket.
- **A. Map/exec only in GM-600; network and writes as a follow-up task
  (recommended).** Benefit: ships the proven fix with the smallest
  breakage surface; matches the AC. Risk: ADR 0033 stays enforced by
  configuration only for now.
- **B. Add `(deny network-outbound)` now.** Benefit: ADR 0033's network
  half becomes OS-enforced for one rule. Risk: rust-analyzer or cargo
  touching a local socket (e.g. a user's proxy settings) fails in ways the
  current tests do not cover.

## Appendix: g-mesh calls used

Project `g-mesh-wt-gm600` (index cold at start; rows carried
`provenance: semanticTier absent`, which only affects method calls through
variable receivers - none of the questions below depend on one).

- `get_file_outline plugins/rust/src/semantic.rs` -> `engine` (104),
  `CheckedBridge::checked` (178), `Toolchain` (226), `detect` (245),
  `pinned_env` (487), `LoaderCheck` (529), `cargo_env_loader_var` (592),
  `cargo_home` (687), `resolve` (724) (0-based): the edit map's lines.
- `find_callers LspClient::start` -> `LspBridge::ensure_client`
  (`plugins/sdk/src/lsp/bridge.rs`) and one client test: the server spawn
  is `LspClient::start` (`client.rs:233` `Command::new(&config.command)`),
  reached only through the bridge, so rewriting `SemanticConfig` in
  `engine` covers it.
- `find_callers LspBridge::new` -> `engine` in the rust, python and
  typescript plugins, 4 bridge unit tests, 2 overload tests, and
  `project_extra_env::index_fixture` in the rust tests: the rust launch site
  is `semantic::engine` alone; the sandbox stays in the Rust plugin.
- `find_references cargo_env_loader_var` (ambiguous: function vs test
  module; re-run by id `5216e5…`) -> `engine` and `CheckedBridge::checked`,
  both in `plugins/rust/src/semantic.rs`: what references the GM-587
  pre-check.
- `find_callers semantic::engine` (id `c6b72d…`, `answer: files`) ->
  `tests/indexing_writes_nothing_and_stays_offline.rs`,
  `tests/semantic_runs_no_project_code.rs`, plus the factory reference in
  `src/main.rs`: files the `engine` split touches or must keep compiling.
- `search_code` (spawn the language server) -> no result: the embedding
  pass had not started; `grep Command::new|.spawn()` over
  `plugins/sdk/src` and `plugins/rust/src` located `client.rs:233` and
  `resolve.rs:149` instead.
