# GM-601: run rust-analyzer under a Linux sandbox (Landlock + noexec mount namespace)

Status: design (slice S1), awaiting owner decisions D1-D5.
Branch: `feat/GM-601-linux-landlock-sandbox` (off `release-4.14.0`).
Parent: GM-587 D2 ("Split per OS"); macOS counterpart: GM-600 (Seatbelt).
The code slice starts only after GM-600 is merged into the release branch.

## Question

GM-587 shipped a refuse-to-structural pre-check (`cargo_env_loader_var`,
called from `engine` and `CheckedBridge::checked`) for the one proven
channel: a project `.cargo/config.toml` `[env]` loader variable
(`LD_PRELOAD` on Linux) that cargo applies with `force = true` to the
`rustc` children of `cargo metadata`. GM-587 D2 chose an OS sandbox of the
whole rust-analyzer process tree as the structural fix, because it closes
AC3 channels 1-5 as a class. This slice designs the Linux half: with the
pre-check disabled, indexing a project whose cargo config preloads a
project shared object must fire no marker, and the semantic tier must
still work.

## Finding that reshapes the task: Landlock alone cannot block `LD_PRELOAD`/`dlopen`

The task names Landlock as the mechanism. It is not sufficient on its own,
and `bwrap` cannot fill the gap. Precisely:

- **What Landlock's `LANDLOCK_ACCESS_FS_EXECUTE` covers.** It is checked
  when a file is opened for execution (`FMODE_EXEC`): the program of an
  `execve(2)`/`execveat(2)`, its ELF interpreter (`ld-linux*.so`) and a
  script's `#!` interpreter. It is **not** checked by `mmap(2)` or
  `mprotect(2)` with `PROT_EXEC`; Landlock has no mmap hook.
- **How a shared object is loaded.** `ld.so` loads `DT_NEEDED`,
  `LD_PRELOAD`, `LD_AUDIT` and `dlopen` libraries with
  `open(O_RDONLY)` + `mmap(PROT_READ|PROT_EXEC)`. Landlock sees only
  `LANDLOCK_ACCESS_FS_READ_FILE`.
- **Why `READ_FILE` cannot close it.** Landlock rules only grant; a grant
  beneath a directory covers every file under it. rust-analyzer must read
  the project's sources, so the project root needs `READ_FILE`, and with it
  every `.so` the project ships. Per-file grants (every non-ELF file
  enumerated at launch) are possible but break on files created after
  launch (rust-analyzer would be denied a new `.rs`), and cost a rule per
  file. `AT_EXECVE_CHECK` (kernel 6.14) is opt-in for script interpreters;
  glibc's `ld.so` does not consult it for preload/`dlopen`.
- **What closes it: a `noexec` mount.** `mmap(PROT_EXEC)` of a file whose
  mount is `noexec` fails with `EPERM` (`do_mmap` checks `path_noexec`), and
  a later `mprotect(PROT_EXEC)` fails too (`VM_MAYEXEC` is cleared at map
  time). `execve` of a file on that mount fails with `EACCES`. An
  unprivileged process gets such a mount by entering a new user + mount
  namespace and remounting the path `noexec`.
- **Measured** (Docker Desktop, kernel `6.10.14-linuxkit`, `rust:1.97.1`,
  `--security-opt seccomp=unconfined`; script in the slice scratchpad
  `gm601/run.sh`, marker `.so` with a constructor appending to `/out/MARK`):

  | Arm | Result |
  |---|---|
  | A: `LD_PRELOAD=/proj/x.so /bin/true` | MARK 1 line (fires) |
  | B: `unshare -Urm`, bind `/proj` onto itself, remount `bind,noexec`, same preload and a `ctypes.CDLL("/proj/x.so")` | `ld.so: object '/proj/x.so' from LD_PRELOAD cannot be preloaded (failed to map segment from shared object): ignored.`, `OSError: ... failed to map segment`, MARK 0; `/bin/true` still ran |
  | B': same namespace, exec a copy of `/bin/true` under `/proj` | `Permission denied`, rc 126 |
  | C: `bwrap --dev-bind / / --unshare-user` with the preload | MARK 2 lines (bwrap itself and `true` loaded it) |

  `ld.so` ignores the failed preload and the program continues, so the
  semantic tier keeps working (the `rustc` children run, minus the marker).
- **`bwrap` (0.12.0) has no `noexec` bind option** (`--help` offers only
  `--remount-ro` and `--exec-label`), so a `bwrap` jail cannot deny the
  mapping either (arm C). Its one unique property - working setuid where
  unprivileged user namespaces are off - does not help, because setuid
  `bwrap` refuses `--cap-add`, so nothing inside can add `noexec`.

So the Linux design is **two layers**, both applied in the child before
`exec` of rust-analyzer and inherited by its whole process tree:

1. **Mount layer (the closer):** user + mount namespace, every mount made
   `noexec` except an exec allowlist of system and toolchain prefixes.
   Closes `LD_PRELOAD`/`LD_AUDIT`/`dlopen`/`execve` from the project,
   `CARGO_HOME/registry`, `CARGO_HOME/git`, `$HOME`, `/tmp`.
2. **Landlock layer (defense in depth):** `EXECUTE` allowlisted to the same
   prefixes (catches an `execve` from any mount the mount layer missed),
   TCP bind/connect denied (ABI 4), abstract-unix-socket and signal scoping
   (ABI 6). Applied best-effort by ABI.

`bwrap` is dropped (D1). This changes the task's second acceptance
criterion; D1 and D2 put that to the owner.

## 1. Rule sets, capability probe, where they apply

### Exec allowlist (one list, both layers)

Built in the parent from canonicalized, deduplicated paths that exist:

- system: `/usr`, `/bin`, `/sbin`, `/lib`, `/lib32`, `/lib64`, `/libx32`
  (symlinks on merged-`/usr` resolve into `/usr`), `/nix/store` if present;
- the pinned toolchain: `Toolchain.sysroot` (`$RUSTUP_HOME/toolchains/<pin>`),
  the resolved rust-analyzer binary's directory, the `rustup` binary's
  directory and `$CARGO_HOME/bin` (rustup proxies; GM-587 R0 shows
  rust-analyzer runs `rustup which cargo`).

Never allowed: the project root, a target dir outside it,
`$CARGO_HOME/registry`, `$CARGO_HOME/git`, `$HOME` otherwise, `/tmp`,
`/var/tmp`, `/dev/shm`, `/run/user/<uid>`. A toolchain elsewhere (`/opt`,
`asdf`/`mise` installs) is allowed by being the sysroot or the binary's
directory; a C linker is not needed (nothing links: build scripts and proc
macros are off).

### Mount layer (in `pre_exec`, in this order)

1. `unshare(CLONE_NEWUSER | CLONE_NEWNS)`.
2. Write `deny` to `/proc/self/setgroups`, then `"<uid> <uid> 1"` to
   `/proc/self/uid_map` and `"<gid> <gid> 1"` to `/proc/self/gid_map`
   (identity map: file ownership is unchanged; the strings are formatted in
   the parent).
3. `mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)` - no propagation out.
4. `mount_setattr("/", AT_RECURSIVE, {attr_set: MOUNT_ATTR_NOEXEC})`
   (kernel >= 5.12) - default deny.
5. For each allowlisted prefix: `mount(P, P, MS_BIND | MS_REC)` then
   `mount_setattr(P, AT_RECURSIVE, {attr_clr: MOUNT_ATTR_NOEXEC})`. Only
   the `noexec` this process added is cleared; a mount inherited already
   `noexec` stays locked (`MNT_LOCK_NOEXEC`), which is correct.

Step 5's exact mechanics (bind of a prefix that is not a mount point,
re-clearing beneath a recursive attribute) are the code slice's first spike,
proved in Docker before anything else is written. Fallback if the
allowlist shape fails there: the deny-list shape (D3 option B).

### Landlock layer (after the mount layer: a Landlock domain forbids mount changes)

- Probe: `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)`
  -> ABI number, or `ENOSYS`/`EOPNOTSUPP` = unavailable.
- `handled_access_fs = EXECUTE`; `PathBeneath` rule granting `EXECUTE` on
  each allowlisted prefix. `READ_*` is not handled (reads stay open; the
  read surface is not the threat, and handling it means allowlisting
  `/etc`, `/proc`, `/dev`, the project...). Writes: D4.
- ABI >= 4: `handled_access_net = BIND_TCP | CONNECT_TCP`, no rules (all
  TCP denied; rust-analyzer talks over stdio and cargo is offline).
- ABI >= 6: `scoped = ABSTRACT_UNIX_SOCKET | SIGNAL`.
- In `pre_exec`: `prctl(PR_SET_NO_NEW_PRIVS, 1)`, then
  `landlock_restrict_self(fd, 0)`.

### Crate and async-signal safety

`pre_exec` runs between `fork` and `exec` in a multi-threaded parent, so
it may only make raw syscalls: no allocation, no locks, no `Path` ->
`CString` conversion. Everything is prepared in the parent and moved into
the closure: the uid/gid map strings, the allowlist as `CString`s, the
Landlock ruleset fd.

- **Landlock:** the `landlock` crate (best-effort ABI compatibility,
  `PathBeneath`, `RulesetCreated`), built in the parent. In the child,
  restrict with a raw `libc::syscall(SYS_landlock_restrict_self, fd, 0)`
  on the prepared fd rather than `RulesetCreated::restrict_self()`, unless
  the code slice confirms the latter allocates nothing. The ruleset fd is
  `O_CLOEXEC`, so rust-analyzer does not inherit it.
- **Namespaces/mounts:** `libc` (`unshare`, `mount`, `syscall(SYS_mount_setattr)`,
  `open`/`write`/`close`, `prctl`).
- Both as `[target.'cfg(target_os = "linux")'.dependencies]` of
  `g-mesh-plugin-rust`; neither is in the workspace today (`landlock` pulls
  `enumflags2`, `thiserror`, `libc`).

### Capability probe and tier selection

At `engine()` time, once per process, the plugin spawns `/bin/true` through
the same `pre_exec` with each step tagged, and maps the first failing step
to a reason (a failing `pre_exec` only surfaces an errno, so the probe runs
the steps one at a time in a throwaway child). A pure function picks the
tier from `(landlock_abi, mount_layer_result)`:

| Landlock | user+mount ns, `noexec` | Tier | Log line (semantic tier start) |
|---|---|---|---|
| ABI >= 1 | works | **full** | `sandbox: noexec mounts + Landlock ABI <n>` |
| missing | works | **mount-only** | `sandbox: noexec mounts; Landlock unavailable (<errno>)` |
| ABI >= 1 | fails | D2 | D2 |
| missing | fails | **structural** (engine `Err`) | `semantic tier refused: no sandbox (<reason>)` |

The mount layer fails on: `kernel.unprivileged_userns_clone=0` (Debian),
`user.max_user_namespaces=0`, Ubuntu >= 23.10's
`kernel.apparmor_restrict_unprivileged_userns=1` (the `unshare` succeeds
but the namespace has no capabilities, so the mount fails `EPERM`), a
kernel < 5.12 (no `mount_setattr`), and Docker's default seccomp profile
(measured above: `unshare(CLONE_NEWUSER)` -> `EPERM` without
`seccomp=unconfined`).

A refusal goes through the existing path: `engine` returns `Err`,
`LazyEngine` (`plugins/sdk/src/semantic.rs:223`) logs it and the plugin
stays structural for the process.

## 2. bwrap fallback

Dropped, with evidence (arm C, and no `noexec` option in 0.12.0). A
`bwrap` launcher would add a dependency and a code path that leaves the
measured channel open. Where Landlock is missing the mount layer still
runs (mount-only tier); where the mount layer cannot run, there is no
`bwrap` configuration that closes the channel, so the outcome is D2's.
The GM-587 pre-check stays in every tier as defense in depth.

## 3. Launch abstraction shared with GM-600 (assumed interface)

The server is spawned in exactly one place, `LspClient::start`
(`plugins/sdk/src/lsp/client.rs:225-296`), reached from
`LspBridge::ensure_client` (`plugins/sdk/src/lsp/bridge.rs:706`) - which
also respawns a crashed server - and one SDK unit test. `SemanticConfig`
derives `PartialEq, Eq`, so the sandbox is **not** a config field; it rides
on the bridge like the existing builders (`quiescent_signal`,
`client_watches_files`). Minimal interface:

```rust
// plugins/sdk/src/lsp/sandbox.rs (new, OS-independent)
pub trait ServerSandbox: Send + Sync + std::fmt::Debug {
    /// The command that starts `program args` confined. The caller adds
    /// env, current dir and stdio. Called on every (re)spawn.
    fn command(&self, program: &Path, args: &[String]) -> anyhow::Result<std::process::Command>;
    /// One line for the semantic-tier log, e.g. "noexec mounts + Landlock ABI 6".
    fn describe(&self) -> String;
}

impl LspBridge {
    pub fn sandbox(mut self, sandbox: Arc<dyn ServerSandbox>) -> Self;
}
// LspClient::start(..., sandbox: Option<&dyn ServerSandbox>)
//   -> Command::new(program) when None, sandbox.command(program, args) otherwise.
```

- macOS (GM-600) returns `Command::new("/usr/bin/sandbox-exec")` with
  `-p <profile> <program> <args...>`; Linux returns `Command::new(program)`
  with `args` and an `unsafe pre_exec` closure. `command()` is the only
  shape that serves both: std's `Command` cannot change its program after
  construction, and a `pre_exec` cannot rewrite argv.
- The plugin side: `plugins/rust/src/sandbox/{mod,linux,macos}.rs`,
  `sandbox::for_server(root, &toolchain) -> Result<Option<Arc<dyn ServerSandbox>>>`
  (`Err` = refuse to structural with the reason; `Ok(None)` = Windows).
- Linux builds a fresh Landlock ruleset per `command()` call, so a respawn
  is confined like the first start.
- Reconcile at review: if GM-600 lands a different trait name or a config
  field, GM-601 adopts it; what GM-601 needs is only "produce the `Command`
  for program+args, called per spawn, may fail".

## 4. Test route

- **Local:** Docker Desktop 28.3.2 runs (`linux x86_64`, kernel
  `6.10.14-linuxkit`); no podman/colima/lima/orbstack. Its kernel is built
  **without Landlock** (`/proc/config.gz`: `# CONFIG_SECURITY_LANDLOCK is
  not set`; `CONFIG_LSM="yama,loadpin,safesetid,integrity,bpf"`). With
  `--security-opt seccomp=unconfined` the user namespace and `noexec`
  remount work (arm B). So locally: the mount layer, the mount-only tier,
  the marker fixture and every non-Landlock control, in `rust:1.97.1`
  (already pulled) with a named volume for `target/`.
- **CI:** the `x86_64-unknown-linux-gnu` row runs on `ubuntu-22.04`, and
  the `plugin-rust` test section already runs
  `semantic_runs_no_project_code` there. Ubuntu's kernel lists `landlock`
  in its LSMs and allows unprivileged user namespaces on 22.04, so the
  full tier is expected; the "Runner state" step should print the Landlock
  ABI and the userns sysctls, and the test asserts the full tier when
  `GITHUB_ACTIONS` is set so a degraded runner fails loudly instead of
  skipping. Moving that row to `ubuntu-24.04` would put it under the
  AppArmor userns restriction (D2's tier).
- **Recommendation (D5):** Docker locally for development and the
  non-Landlock controls; CI `ubuntu-22.04` for the acceptance run and the
  two Landlock-only controls, via `workflow_dispatch` with
  `target=x86_64-unknown-linux-gnu` on a throwaway control branch (needs
  the owner's push approval each time).

## 5. Fixture

Reuse `mod cargo_env_loader_var` in
`plugins/rust/tests/semantic_runs_no_project_code.rs` (`:888`): a
`[workspace]` with one `app` package, `build_marker_dylib` (`:346`)
producing `dylib/marker.so` whose constructor appends to
`<root>/MARK_cargo_env_loader`, and `.cargo/config.toml`
`[env] LD_PRELOAD = { value = "<root>/dylib/marker.so", force = true }`.
The existing sanity step (an unsandboxed `cargo metadata` fires the marker)
stays. New in a `#[cfg(target_os = "linux")] mod linux_sandbox`:

- The engine is built through a hidden test entry,
  `semantic::engine_with(root, EngineOptions { loader_precheck: false, sandbox: SandboxMode::... , extra_exec_allowed: Vec<PathBuf> })`
  (D4), reaching both `engine` and `CheckedBridge::checked`.
- The marker writes into the project root. That is only valid while the
  sandbox does not confine writes (D4 recommendation); if writes are
  confined, the marker path moves to a write-allowed directory, and B3
  below is what proves the marker can still write.
- "Semantic tier still works" reuses the resolved-method check the
  extra-env tests use (`resolved_method`, `:725`): `main`'s
  `util::Thing.method()` resolves to `util.rs`.

## 6. Edit map

Line numbers are 1-based, from this worktree.

Change:

- `plugins/sdk/src/lsp/sandbox.rs` (new): `ServerSandbox` (section 3).
  Register in `plugins/sdk/src/lsp/mod.rs`; re-export from the SDK root.
- `plugins/sdk/src/lsp/client.rs` `LspClient::start` (`:225-296`): new
  `sandbox: Option<&dyn ServerSandbox>` parameter; build `command` from it
  (line `:233`), everything after unchanged.
- `plugins/sdk/src/lsp/bridge.rs`: `LspBridge` gains a
  `sandbox: Option<Arc<dyn ServerSandbox>>` field and a builder beside
  `quiescent_signal` (`:602`) / `client_watches_files` (`:623`);
  `ensure_client` (`:706`) passes it to `LspClient::start`. The SDK test
  `the_server_runs_with_no_current_directory_exe_lookup_whatever_the_manifest_says`
  (`client.rs:916`) passes `None`.
- `plugins/rust/src/sandbox/mod.rs`, `linux.rs` (new): allowlist builder,
  probe, `select_tier` (pure), `LinuxSandbox::command` with the `pre_exec`
  closure; `macos.rs` is GM-600's.
- `plugins/rust/src/semantic.rs` `engine` (`:105-149`): after
  `config.env.extend(pinned_env(&toolchain))`, call
  `sandbox::for_server(root, &toolchain)`; `Err` -> `bail!` (structural,
  logged); `Some` -> `.sandbox(...)` on the `LspBridge` (`:147`) and add
  `describe()` to the existing semantic-tier log line. Split into
  `engine_with(root, EngineOptions)` (hidden) with `engine` calling it
  with defaults.
- `plugins/rust/src/semantic.rs` `CheckedBridge` (`:165-170`), `new`
  (`:173-175`), `checked` (`:179-188`): carry `loader_precheck` so the
  test entry can disable the re-check too.
- `plugins/rust/Cargo.toml`: Linux-only `landlock`, `libc`.
- `.github/workflows/ci.yml` "Runner state" (`:318-330`): on Linux print
  `uname -r`, `/sys/kernel/security/lsm`, the Landlock ABI, and the userns
  sysctls.
- `scripts/test-linux-sandbox-docker.sh` (new, shellcheck-clean): runs the
  `linux_sandbox` tests in `rust:1.97.1` with `seccomp=unconfined` and a
  cached target volume.
- `docs/adr/`: new ADR "the semantic tier runs rust-analyzer in an OS
  sandbox" (shared with GM-600), linked from `engine`'s doc comment.

Read (not changed): `Toolchain` / `Toolchain::detect` (`semantic.rs:227-243`,
`:246-272`) for `sysroot`, `rustup`, `rust_analyzer`; `cargo_home`
(`:688-696`); `refusal` (`:546-562`); `LazyEngine`
(`plugins/sdk/src/semantic.rs:223-240`); the test helpers `in_its_env`
(`:192`), `child_fixture` (`:219`), `marker_dylib` (`:337`),
`loader_fixture_in_its_env` (`:904`).

Out of scope: `probe` (`plugins/sdk/src/lsp/resolve.rs:148`) runs
`rust-analyzer --version` unsandboxed; it starts no cargo and reads no
project config. `Toolchain::detect` runs `rustup` in a neutral dir.

## 7. Behaviours to pin and controls

Behaviours (the tests slice pins each):

- **B1** Linux, pre-check off, sandbox on (full tier on CI, mount-only in
  Docker): indexing the `[env] LD_PRELOAD` fixture writes no marker, and
  `util::Thing.method()` resolves (semantic tier works).
- **B2** Same fixture, sandbox off: the marker fires (in-test sanity arm;
  this is the task's control AC).
- **B3** Sandbox on, `LD_PRELOAD` pointing at a copy of the marker in an
  `extra_exec_allowed` directory: the marker fires - proves the marker can
  load and write from inside the sandbox, so B1's silence is the denied
  mapping.
- **B4** `LinuxSandbox::command` running a copy of `/bin/true` placed in
  the project root fails (`EACCES`/126) - exec from the project is denied.
- **B5** Landlock layer alone (mount layer disabled by a test option): an
  `execve` of a binary outside the allowlist fails. Landlock kernels only.
- **B6** `select_tier`: Landlock missing + mount layer works -> mount-only
  with the logged reason; both fail -> structural `Err` naming the reason;
  Landlock present + mount layer fails -> D2's choice.
- **B7** On CI (`GITHUB_ACTIONS` set) the probe reports the full tier;
  otherwise the test fails.

Controls (7; the verify slice reverts code, never the test, in a
throwaway worktree; C1-C3, C6, C7 build in the Docker route, C4-C5 on CI):

1. **C1** `LspClient::start` ignores `sandbox` -> B1 fails (marker).
2. **C2** skip the mount layer's `noexec` steps (keep userns + Landlock) ->
   B1 fails on CI: pins the claim that Landlock alone does not block the
   preload.
3. **C3** drop the toolchain prefixes from the allowlist -> B1's
   "resolves" assertion fails (the semantic-tier check has teeth).
4. **C4** Landlock ruleset handles no access right -> B5 fails (CI).
5. **C5** the probe always reports Landlock missing -> B7 fails (CI).
6. **C6** `select_tier` maps "mount layer fails" to mount-only -> B6 fails
   (`--lib`).
7. **C7** don't add `extra_exec_allowed` to the allowlist -> B3 fails.

C1, C3, C7 share the integration binary; C6 is `--lib`; C2/C4/C5 go in
one CI dispatch.

## 8. Owner decisions

Answered 2026-10-20 (owner's chosen labels quoted):

- D1: "noexec-mount + Landlock, без bwrap (Recommended)" - option A; AC2
  amended accordingly.
- D2: "Только Landlock + лог (Recommended)" - option A.
- D3: "Default-deny + allowlist (Recommended)" - option A.
- D4: not asked separately; applied as option A because it is the same
  question GM-600 D4 answered ("Только загрузка/запуск, сеть — отдельной
  задачей (Recommended)"); write/network confinement goes to the follow-up
  task. The test entry follows whatever GM-600 ships (its `server_config`
  split), not a new `engine_with`.
- D5: "Docker + CI ubuntu-22.04 (Recommended)" - option A.
- Section 3's assumed interface is superseded by GM-600's
  `sandbox::{SandboxPolicy, SandboxOutcome, confine}`; a short design delta
  after GM-600 merges decides how the Linux backend applies `pre_exec`
  through it (an SDK spawn hook vs a self-exec trampoline).

### D1 - mechanism: noexec mount namespace + Landlock, no bwrap (recommended)

- **Today:** on Linux nothing confines rust-analyzer; only the GM-587
  pre-check refuses the one proven `[env]` channel. The task asks for
  Landlock with a `bwrap` fallback.
- **Change:** close the load channel with a user+mount namespace that makes
  every mount `noexec` except the toolchain/system allowlist; add Landlock
  (exec allowlist, no TCP) on top; drop `bwrap`. Amend AC2 to "Kernels
  without Landlock run the mount layer alone; where the mount layer cannot
  run, see D2; the reason is logged".
- **Example:** a clone with `[env] LD_PRELOAD = ./dylib/marker.so` indexes
  semantically; every `rustc` prints `cannot be preloaded (failed to map
  segment)` and continues; no marker.
- **Options:**
  - **A (recommended):** as above. Benefit: the only measured mechanism
    that blocks the mapping (arm B), no new runtime dependency, works
    without Landlock. Risk: needs unprivileged user namespaces, which some
    distributions restrict (D2); allowlist upkeep as toolchains move.
  - **B:** Landlock + `bwrap` as the task states. Benefit: matches the
    ticket. Risk: closes nothing for the measured channel (arm C, no
    `noexec` in `bwrap`); AC1 cannot pass.
  - **C:** Landlock with per-file `READ_FILE` grants (no ELF in the
    project readable). Benefit: no user namespace needed. Risk: files
    created after launch are unreadable to rust-analyzer; one rule per
    file; complex.

### D2 - hosts with Landlock but no usable user namespace (Ubuntu >= 23.10 default, containers)

- **Today:** every Linux user gets the semantic tier with the pre-check
  only.
- **Change:** pick the tier where Landlock works but the mount layer does
  not.
- **Example:** an Ubuntu 24.04 desktop: `apparmor_restrict_unprivileged_userns=1`,
  Landlock ABI 4+.
- **Options:**
  - **A (recommended): Landlock-only partial tier**, logged as "sandbox:
    Landlock ABI <n> only; shared-object loads from the project are not
    blocked (<reason>)", with the GM-587 pre-check still refusing the proven
    `[env]` channel. Benefit: strictly better than today, keeps semantic
    Rust on the most common desktop distribution. Risk: a not-yet-known
    load channel (AC3 2-5 are closed only by precedence or dormant) stays
    open there.
  - **B: refuse to structural.** Benefit: matches the task's "degrade"
    wording, no unconfined load path anywhere. Risk: most Ubuntu 24.04
    users and every default Docker container lose semantic Rust.
  - **C: ship an AppArmor profile granting g-mesh's plugin `userns`.**
    Benefit: full tier on Ubuntu. Risk: needs root at install time and a
    profile per distribution; separate task.

### D3 - shape of the mount layer

- **Today:** none.
- **Change:** choose default-deny or deny-list `noexec`.
- **Example:** a `.so` the project's `[env]` points at in
  `~/.cargo/registry/src/.../vendored.so` (from a previously fetched crate).
- **Options:**
  - **A (recommended): default-deny** - everything `noexec`, allowlist
    re-enabled (the GM-587 R9 shape GM-600 also uses). Benefit: a path
    nobody thought of is denied. Risk: a toolchain outside the allowlist
    fails to start (logged; the semantic tier is lost on that host until
    the list grows); depends on `mount_setattr` (kernel >= 5.12).
  - **B: deny-list** - `noexec` on the project root, external target dir,
    `$CARGO_HOME/{registry,git}`, `$HOME`, `/tmp`, `/var/tmp`, `/dev/shm`,
    `/run/user/<uid>`, with the toolchain re-enabled under `$HOME`.
    Benefit: never breaks an unusual toolchain; works on older kernels.
    Risk: a writable location not on the list (e.g. a second disk mounted
    under `/mnt`) stays exec-able.

### D4 - Landlock write confinement, and the test entry

- **Today:** ADR 0033 "write nothing" is held by configuration (rust-analyzer
  1.97.1 passes `-Zlockfile-path`; no configuration of ours prevents an
  older one writing `Cargo.lock`).
- **Change:** decide whether Landlock also handles write rights in this
  task.
- **Example:** with writes confined to a per-server temp dir, an older
  rust-analyzer's `Cargo.lock` write fails instead of landing in the
  project.
- **Options:**
  - **A (recommended): not in this task** - exec + network + scoping only;
    a follow-up measures what rust-analyzer, cargo and rustup write
    (lockfile path, `~/.cache`, rustup's `tmp`) before confining. Benefit:
    no unmeasured breakage; the load is already blocked, so injected code
    never runs to write. Risk: "write nothing" stays enforced by
    configuration only.
  - **B: confine writes now** to a per-server temp dir and the
    `-Zlockfile-path` target. Benefit: enforces ADR 0033 by mechanism.
    Risk: an unlisted write path breaks indexing on some setups; the
    marker fixture must move its `MARK` out of the project.
- **Test entry (same decision, sub-question):** recommended a hidden
  `pub fn engine_with(root, EngineOptions)` that the factory never calls.
  Rejected: an environment variable (a shipped switch that turns the
  pre-check and sandbox off for anyone who sets it) and a cargo feature
  (a separate test build of the plugin, a second build in every control).

### D5 - test route

- **Today:** this host is macOS; Docker Desktop's kernel has no Landlock;
  CI's Linux row is `ubuntu-22.04`.
- **Change:** where the Linux tests and controls run.
- **Options:**
  - **A (recommended):** Docker locally (mount layer, mount-only tier,
    C1, C3, C6, C7) + CI `ubuntu-22.04` for the acceptance run and C2, C4,
    C5 via `workflow_dispatch` on a throwaway branch. Benefit: no new
    tooling; CI is where the full tier ships. Risk: CI controls need a push
    the owner approves each time; a CI round trip per control batch.
  - **B:** install a Landlock-capable VM locally (`colima`/`lima` with an
    Ubuntu kernel). Benefit: every control local, no pushes. Risk: a new
    VM (~1-2 GB, setup time) to maintain on this machine.

## Appendix: g-mesh calls used

All against `select_project /Users/Valentin_Taiurskii/Projects/ClaudeProjects/g-mesh-wt-gm601`,
with `project: "g-mesh-wt-gm601"` on each call (other agents shared the
connection). The worktree's index was structural-only during this slice
(`provenance: semanticTier absent`); the answers below are direct
function-call edges, which the structural tier carries.

- `get_file_outline plugins/rust/src/semantic.rs` -> `engine`, `CheckedBridge`
  (+`new`, `checked`), `Toolchain`/`detect`, `pinned_env`, `LOADER_VARS`,
  `refusal`, `cargo_env_loader_var`, `cargo_home`, test module bounds.
- `find_definition engine` (id `c6b72de3…`) -> the launch body: the
  pre-check `cargo_env_loader_var(root).refusal()` precedes `resolve`, and
  the server is built by `LspBridge::new(...).quiescent_signal().client_watches_files()`.
- `find_references cargo_env_loader_var` (id `5216e506…`) -> **what
  references the GM-587 pre-check:** `semantic::engine` and
  `semantic::CheckedBridge::checked`, both in `plugins/rust/src/semantic.rs`
  (complete, `hasMore: false`).
- `find_definition pinned_env` -> doc comment confirming the `[env]` gap is
  handled by the refusal in `engine`.
- `search_code "spawn the language server child process"` -> no answer
  (embedding pass not started); fell back to `grep Command::new|spawn` over
  `plugins/sdk/src`, which found `LspClient::start` and `resolve::probe`.
- `get_file_outline plugins/sdk/src/lsp/client.rs` -> `LspClient::start`
  (`:225`).
- `find_definition LspClient::start` (id `3e9bb2ca…`) -> the one
  `Command::new(&config.command)` that spawns the server.
- `find_callers LspClient::start` -> **where rust-analyzer is spawned
  from:** `LspBridge::ensure_client` (`plugins/sdk/src/lsp/bridge.rs:706`)
  and the SDK test `the_server_runs_with_no_current_directory_exe_lookup_whatever_the_manifest_says`
  (complete).
- `find_definition SemanticConfig` / `LazyEngine` -> locations; `SemanticConfig`
  derives `PartialEq, Eq` (read by grep at `config.rs:233`), which rules out
  a trait-object field there.
- **Files the launch change touches** (from the above): `plugins/sdk/src/lsp/client.rs`,
  `plugins/sdk/src/lsp/bridge.rs`, new `plugins/sdk/src/lsp/sandbox.rs`,
  `plugins/rust/src/semantic.rs`, new `plugins/rust/src/sandbox/`,
  `plugins/rust/Cargo.toml`, `plugins/rust/tests/semantic_runs_no_project_code.rs`.
