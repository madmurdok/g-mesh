# GM-601: run rust-analyzer under a Linux sandbox (Landlock + noexec mount namespace)

Status: design approved (D1-D5, section 8); design delta S6 (sections 3, 3a, 6, 7) after GM-600 merged.
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

### Mount layer (in the trampoline, section 3a; in this order)

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

## 3. Launch: through GM-600's `confine` (design delta, slice S6)

The interface section 3 assumed (`ServerSandbox` trait, `LspBridge::sandbox`)
is superseded by what GM-600 merged (`2f7e8f5`):

- `plugins/rust/src/sandbox.rs`: `SandboxPolicy { project_root, toolchain }`
  (`:29-36`), `SandboxPolicy::for_server` (`:74-86`),
  `confine(&mut SemanticConfig, &SandboxPolicy) -> SandboxOutcome`
  (`:91-101`) which dispatches to `mod seatbelt` on macOS and returns
  `NoBackend` elsewhere; the backend rewrites `config.command/args` (argv
  wrap) after a preflight `lsp::probe(<wrapped> --version)`.
- `plugins/rust/src/semantic.rs`: `server_config(root)` (`:143-175`) builds
  the policy and calls `admit(sandbox::confine(..), &policy)` (`:181-203`):
  `Confined` and `NoBackend` are logged and admitted, `Failed` is `Err`
  (structural tier).
- `confine`'s only callers are `server_config` and the macOS unit test
  helper `sandbox::tests::confine_server` (g-mesh, appendix). The server is
  spawned only by `LspClient::start` (SDK), from `LspBridge::ensure_client`
  and one SDK unit test; `ensure_client` reuses the same `SemanticConfig` on
  every respawn.

The Linux layers need code in the child between `fork` and `exec` (unshare
user+mount namespaces, `mount_setattr` noexec, Landlock `restrict_self`).
An argv rewrite cannot express a `pre_exec` closure directly, hence 3a.

## 3a. Route through `confine`

### Options

- **(a) SDK spawn hook.** `SemanticConfig` (or `LspBridge`) gains an
  optional pre-exec hook that `LspClient::start` installs with
  `CommandExt::pre_exec`.
- **(b) Self-exec trampoline.** Linux `confine` rewrites argv to
  `/proc/self/exe __g-mesh-sandbox <layers> <policy...> -- <rust-analyzer> <args...>`.
  The plugin's `main` recognises the hidden first argument before it calls
  the SDK's `run`, sets the layers up in-process, and `execve`s
  rust-analyzer in place (same pid, as `sandbox-exec` does on macOS).

| | (a) SDK spawn hook | (b) trampoline |
|---|---|---|
| Async-signal safety | The closure runs after `fork` in the multi-threaded plugin (SDK reader threads, the core loop): raw syscalls only, no allocation or locks, every `CString`, map string and the Landlock ruleset fd prepared in the parent. The `landlock` crate's `restrict_self` cannot be trusted there; error reporting is an errno only. | The layers run in a fresh, single-threaded process at the top of `main`, before the SDK starts a thread: ordinary Rust, the `landlock` crate as-is, allocation, readable error text on stderr. `unshare(CLONE_NEWUSER)` requires a single-threaded caller, which this is by construction. |
| Restarts (`ensure_client` reuses the config) | The hook must be re-armed per spawn: a ruleset fd consumed by one child must be rebuilt (`Fn`, not `FnOnce`, plus parent-side state). | Nothing to do: the argv is data; every respawn re-runs the same layers from scratch. |
| Preflight | `lsp::probe(command, args, budget)` takes no hook, so it either grows a hook parameter too or the plugin writes its own probe spawn. | Unchanged GM-600 shape: `lsp::probe("/proc/self/exe", [<trampoline args>, <ra>, "--version"])` runs exactly the layers the server will run. |
| SDK API surface | New public field or builder on `SemanticConfig`/`LspBridge` and a new `LspClient::start`/`probe` parameter. `SemanticConfig` derives `Debug, Clone, PartialEq, Eq` (`config.rs:233`), which a closure field breaks, so a hand-written wrapper type is needed. `SemanticConfig` is referenced from 14 files in 4 crates (sdk, rust, python, typescript; g-mesh appendix), all built via `from_manifest`/parse (no struct literals), so the field compiles everywhere, but it is an SDK interface change for every plugin. | None. Changes stay in `plugins/rust` (`main.rs`, `sandbox.rs`, `semantic.rs`). |
| Testability in Docker | The hook is only reachable through a real spawn; same test shape. | The trampoline is a plain process: `g-mesh-plugin-rust __g-mesh-sandbox ... -- /bin/sh -c ...` can be driven directly from an integration test (`env!("CARGO_BIN_EXE_g-mesh-plugin-rust")`), so B4/B5 need no rust-analyzer. |

### Recommendation: (b) self-exec trampoline

Benefit: no SDK change, no async-signal-safety constraints, restart and
preflight come for free from GM-600's argv-wrap shape, and the trampoline
is directly testable. It mirrors the macOS backend: `sandbox-exec` is a
trampoline the OS ships; on Linux the plugin is its own.

Risks and their handling:

- **Which binary is "self".** The command is the literal `/proc/self/exe`,
  not `current_exe()`'s path: the kernel resolves it to the running
  plugin's inode even after an install refreshes the file on disk
  (`~/.local/share/g-mesh/bin` is overwritten by a release build), where a
  path would respawn the new binary or a `(deleted)` name. A host without
  `/proc` mounted fails the preflight -> `Failed` -> structural.
- **Integration tests are not the plugin binary.** `/proc/self/exe` in the
  test process is the test binary. `server_config(root)` therefore becomes
  `server_config_with(root, launcher: &Path)` (hidden `pub`, `#[doc(hidden)]`)
  with `server_config(root)` passing `/proc/self/exe`; the Linux test
  passes `env!("CARGO_BIN_EXE_g-mesh-plugin-rust")`. On macOS `launcher`
  is ignored. This is a plugin-internal function, not an SDK interface;
  no shipped switch (unlike the rejected environment variable).
- **A hidden argv mode.** `std::env::args()` in the SDK's `run`
  (`run.rs:115`) panics on non-UTF-8, so `main` reads `args_os()` and
  intercepts `__g-mesh-sandbox` first; anyone invoking it only confines a
  process they already could run unconfined, so it grants nothing.
- **Non-UTF-8 paths.** `SemanticConfig.args` is `Vec<String>`; a policy
  path that is not UTF-8 is `Failed` (same rule as `seatbelt::wrapper_args`).
- **Cost.** One extra `execve` per (re)spawn; negligible next to
  rust-analyzer's start-up.

**Owner question: none.** Route (b) changes no interface outside
`plugins/rust`. Route (a) would change the SDK (`SemanticConfig`/
`LspBridge`, `LspClient::start`, `lsp::probe`) and would need the owner;
it is not recommended.

### Trampoline argv and exit codes

```
/proc/self/exe __g-mesh-sandbox --layers mount,landlock|mount|landlock
    --root <project_root> --allow <prefix> [--allow <prefix>...]
    -- <rust-analyzer> <args...>
```

The OS prefixes (`/usr`, `/bin`, `/sbin`, `/lib*`, `/nix/store`, section 1)
are the Linux backend's own, as `OS_MAP`/`OS_EXEC` are Seatbelt's; the
`--allow` list is `policy.toolchain`. `--layers` is mandatory and exact:
the trampoline never drops a layer on its own, so a respawn runs exactly
the tier the preflight proved and logged, and a host that changed under a
running plugin fails closed (the bridge reports the start failure).

Distinct exit codes (the `seatbelt::failure` pattern, matched in the
probe's `exited exit status: N` text): **70** usage error, **71** mount
layer failed (`unshare`/`uid_map`/`mount`/`mount_setattr`, errno and step
on stderr), **72** Landlock layer failed (ABI missing, ruleset or
`restrict_self`), **73** `execve` of the server failed, **74** dropping
capabilities failed. After the layers and before the `execve` the
trampoline clears the bounding and ambient sets, zeroes the effective,
permitted and inheritable sets and sets `no_new_privs`, in every tier: with
the identity uid map a uid-0 plugin (Docker, CI) is root in its user
namespace, and with its capabilities the server could `mount -o
remount,bind,exec` the root in the mount-only tier. A bounding-set drop
refused for lack of `CAP_SETPCAP` (Landlock-only tier, unprivileged user)
is accepted when neither uid is 0, since only a root exec refills the
permitted set from it; otherwise it exits 74.

### Tier selection mapped onto `SandboxOutcome`

Linux `confine` runs a preflight ladder, at most two probes:

1. `--layers mount,landlock` -> success: **full** tier.
2. exit 72 (Landlock missing, mount layer fine) -> retry `--layers mount`
   -> success: **mount-only**.
3. exit 71 (mount layer cannot run) -> retry `--layers landlock`
   -> success: **Landlock-only** (D2).
4. second rung fails, or the first fails with 70/73/74/timeout -> **Failed**.

`fn next_rung(tried: Layers, exit: Option<i32>) -> Option<Layers>` is pure
(unit-tested, `--lib`); the spawning is not.

| Tier | Outcome | `admit` |
|---|---|---|
| full | `Confined { backend: "noexec mounts + Landlock" }` | existing line, Ok |
| mount-only | `Confined { backend: "noexec mounts" }`; `confine` logs one line `sandbox: Landlock unavailable (<stderr reason>)` | existing line, Ok |
| Landlock-only | **new** `Partial { backend: "Landlock", gap: String }` | new line: `rust-analyzer runs under Landlock only; shared-object loads from the project are not blocked (<gap>); the cargo config pre-check still refuses the [env] loader channel`, Ok |
| none | `Failed(reason)` (both rungs' stderr) | existing `Err` -> structural |
| other OS | `NoBackend` | unchanged |

`SandboxOutcome` gets one new variant, `Partial { backend: &'static str, gap: String }`,
rather than a backend string on `Confined`: `admit`'s `Confined` line
states "nothing under <root>" may be mapped, which is false for
Landlock-only (section "Finding"), and an exhaustive `match` makes every
future caller decide what a partial confinement means instead of treating
it as full. Seatbelt never returns it. The variant and `admit`'s arm stay
inside `plugins/rust`.

### Test entry

GM-600's `server_config` is the right entry (D4: no `engine_with`): it
bypasses the cargo config pre-check, which lives in `engine`, and returns
the confined config the test hands to `LspBridge::new`, exactly as the
macOS test does (`semantic_runs_no_project_code.rs:1024-1053`). It does
not suffice unchanged on Linux only because of the launcher (above): the
test calls `server_config_with(&root, Path::new(env!("CARGO_BIN_EXE_g-mesh-plugin-rust")))`
and asserts `config.args[0] == "__g-mesh-sandbox"`. `CheckedBridge`'s
re-check is not on this path (the test builds the bridge itself), so it
needs no option. B3's extra allowed directory is passed by pushing an
`--allow` into the returned `config.args` before `--` (test-side, no
production option).

### What this supersedes in section 1

"Crate and async-signal safety" and "Capability probe and tier selection"
assumed a `pre_exec` closure and an `engine`-time `/bin/true` probe. Under
(b) the layers run in the trampoline (no async-signal constraint; the
`landlock` crate's `restrict_self` is used as-is), the probe is the
preflight ladder above (the real server's `--version`), and the tier table
is the one in this section. The rule sets (exec allowlist, mount steps,
Landlock rights) are unchanged.

## 4. Test route

- **Local:** Docker Desktop 28.3.2 runs (re-checked in S6, 2026-10-20:
  `docker info` -> `28.3.2 linux x86_64`) (`linux x86_64`, kernel
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

- The config is built through GM-600's `server_config` entry (as
  `server_config_with(root, launcher)`, section 3a), which bypasses the
  pre-check; the test builds the bridge itself (D4 as answered).
- The marker writes into the project root. That is only valid while the
  sandbox does not confine writes (D4 recommendation); if writes are
  confined, the marker path moves to a write-allowed directory, and B3
  below is what proves the marker can still write.
- "Semantic tier still works" reuses the resolved-method check the
  extra-env tests use (`resolved_method`, `:725`): `main`'s
  `util::Thing.method()` resolves to `util.rs`.

## 6. Edit map

Line numbers are 1-based, against this worktree after the rebase onto
`release-4.14.0` (GM-600 merged, `2f7e8f5`). Route (b): no SDK file changes.

Change:

- `plugins/rust/src/main.rs` `main` (`:11-24`): first statement
  `if let Some(code) = g_mesh_plugin_rust::sandbox::trampoline(std::env::args_os()) { std::process::exit(code) }`
  (`#[cfg(target_os = "linux")]`; on success `trampoline` never returns,
  it `execve`s). Before `run(...)`, so no SDK thread exists yet.
- `plugins/rust/src/sandbox.rs`:
  - `SandboxOutcome` (`:38-50`): add `Partial { backend: &'static str, gap: String }`.
  - `confine` (`:88-101`): add the `#[cfg(target_os = "linux")]` arm
    `landlock_ns::confine(config, policy, launcher)`; `confine` gains a
    `launcher: &Path` parameter (ignored by `seatbelt::confine`; its one
    other caller, `tests::confine_server` `:317-321`, passes
    `/proc/self/exe`). Module doc (`:1-16`): name the Linux backend.
  - new `#[cfg(target_os = "linux")] pub(crate) mod landlock_ns` after
    `mod seatbelt` (ends `:264`), or a sibling file `sandbox/linux.rs` if
    `sandbox.rs` becomes a directory module (code slice's call; the
    macOS module stays where it is): `OS_EXEC` prefixes,
    `trampoline_args(policy, layers) -> Result<Vec<String>, String>`,
    `confine` (preflight ladder via `lsp::probe`), `next_rung` (pure),
    `failure(detail)` (exit code -> reason, as `seatbelt::failure`).
  - new `#[cfg(target_os = "linux")] pub fn trampoline(args: ArgsOs) -> Option<i32>`:
    parses argv, runs `mount_layer(root, allow)` then `landlock_layer(allow)`
    per `--layers`, then `execvp`-free `execve` of the absolute server
    path; returns the exit code (70-73) on failure, `None` when argv is not
    a trampoline call.
  - new `#[cfg(all(test, target_os = "linux"))] mod linux_tests` beside
    `mod tests` (`:266-447`): `next_rung`, `trampoline_args`, `failure`.
- `plugins/rust/src/semantic.rs`:
  - `server_config` (`:131-175`): body moves to hidden
    `pub fn server_config_with(root: &Path, launcher: &Path)`;
    `server_config(root)` calls it with `Path::new("/proc/self/exe")`.
    The `confine` call (`:173`) passes `launcher`. Doc comment: Linux tiers.
  - `admit` (`:177-203`): new `SandboxOutcome::Partial` arm (log, `Ok`).
  - `engine` doc comment (ends `:111`): Linux sentence beside the macOS one.
  - the two source-reading tests that slice `server_config`'s body by the
    needle `"pub fn server_config("` (`:1207` and
    `the_engine_pins_the_sysroot_and_hints_after_the_pinned_env`
    `:1758-1772`, which also finds `sandbox::confine(&mut config, &policy)`):
    point them at `"pub fn server_config_with("` and the new `confine`
    call text, or they read the one-line wrapper and fail.
- `plugins/rust/Cargo.toml`: `[target.'cfg(target_os = "linux")'.dependencies]`
  `landlock` (exact version computed by `cargo add`, not from memory) and
  `libc` (already in `Cargo.lock` as a transitive dependency).
- `plugins/rust/tests/semantic_runs_no_project_code.rs` `mod cargo_env_loader_var`
  (`:888-1258`): Linux twins of the macOS pair at `:1005-1053`, plus the
  trampoline tests (section 7), all `#[cfg(target_os = "linux")]`.
- `.github/workflows/ci.yml` "Runner state" (`:317-329`): Linux branch
  prints `uname -r`, `/sys/kernel/security/lsm`, the userns sysctls
  (`kernel.unprivileged_userns_clone`, `user.max_user_namespaces`,
  `kernel.apparmor_restrict_unprivileged_userns`, each `|| true`).
- `scripts/test-linux-sandbox-docker.sh` (new, shellcheck-clean):
  `rust:1.97.1`, `--security-opt seccomp=unconfined`, named target volume,
  runs `-p g-mesh-plugin-rust --test semantic_runs_no_project_code` filtered
  to the Linux tests and `--lib sandbox::`.
- `docs/adr/0039-rust-server-runs-under-an-os-sandbox.md` (66 lines):
  Decision gains the Linux backend (trampoline, layers, tiers), Consequences
  the Landlock-only gap (D2) and "no write confinement" (D4).

Read for context (not changed): `sandbox::seatbelt::{confine, preflight, failure}`
(`sandbox.rs:221-264`) as the pattern; `SandboxPolicy::for_server`
(`:74-86`); `Toolchain` (`semantic.rs:281`) / `Toolchain::detect`
(`:300`); `lsp::probe` (`plugins/sdk/src/lsp/resolve.rs:148`, error text
`` `--version` exited {status} ``); `LspClient::start`
(`plugins/sdk/src/lsp/client.rs:225-296`) and `LspBridge::ensure_client`
(`plugins/sdk/src/lsp/bridge.rs:706-750`) for env/cwd/respawn behaviour;
`run` (`plugins/sdk/src/run.rs:113-116`) for argv handling; test helpers
`child_fixture` (`:219`), `in_its_env` (`:192`), `ran_marker` (`:183`),
`build_marker_dylib` (`:346`), `loader_fixture_in_its_env` (`:904`),
`write_live_loader_config` (`:928`), `project_extra_env::resolved_method`
(`:725`), `index_of` (`:712`).

## 7. Behaviours to pin and controls

Where: **Docker** = `scripts/test-linux-sandbox-docker.sh` locally (no
Landlock in Docker Desktop's kernel: mount-only tier); **CI** = the
`x86_64-unknown-linux-gnu` row on `ubuntu-22.04` (full tier). Binaries:
**int** = `--test semantic_runs_no_project_code`, **lib** = `--lib`.

Behaviours (function each lives in):

- **B1** Pre-check bypassed via `server_config_with`, `[env] LD_PRELOAD`
  fixture: no marker, and `util::Thing.method()` resolves.
  `cargo_env_loader_var::a_sandboxed_server_maps_no_project_dylib_and_still_resolves_in_its_env`
  (Linux twin), int; Docker (mount-only) and CI (full).
- **B2** Same fixture, unconfined `cargo metadata`: the marker fires
  (existing sanity arm in `loader_fixture_in_its_env`), int.
- **B3** Confined, marker `.so` copied into an extra `--allow` directory:
  the marker fires - the marker can load and write inside the sandbox, so
  B1's silence is the denied mapping.
  `cargo_env_loader_var::an_allowed_preload_still_loads_under_the_sandbox`, int; Docker + CI.
- **B4** The trampoline with `--layers mount` cannot exec a copy of
  `/bin/true` placed under `--root` (exit 126 from `sh`, `EACCES`).
  `linux_sandbox::the_trampoline_denies_exec_under_the_root`, int; Docker + CI.
- **B5** The trampoline with `--layers landlock` cannot exec a binary
  outside the allowlist. `linux_sandbox::landlock_alone_denies_exec_outside_the_allowlist`,
  int; CI only (skips with a printed reason without Landlock, except
  under `GITHUB_ACTIONS`, where it fails).
- **B6** `next_rung`: 72 after full -> mount-only; 71 after full ->
  Landlock-only; 70/73/`None` -> no rung; a failure on the second rung ->
  no rung. `sandbox::linux_tests::next_rung_*`, lib; Docker.
- **B7** `admit(Partial{..})` is `Ok` and `admit(Failed)` is `Err` naming
  the reason. `semantic::tests::a_partial_sandbox_is_admitted_and_logged`, lib; Docker.
- **B8** On CI, `server_config_with` returns `--layers mount,landlock`
  (full tier). `linux_sandbox::ci_runs_the_full_tier`, int; CI only.

Controls (7; the verify slice reverts code, never the test, in a throwaway
worktree; Docker ones grouped by binary in one script, CI ones in one
`workflow_dispatch` on a throwaway branch the owner approves pushing):

1. **C1** Linux `confine` returns `Confined` without rewriting argv ->
   B1 fails (marker). Docker, int.
2. **C2** `mount_layer` skips the `mount_setattr` noexec steps (userns
   kept) -> B1 fails on CI: pins "Landlock alone does not block the
   preload". CI, int.
3. **C3** drop `policy.toolchain` from `--allow` -> B1's "resolves"
   assertion fails. Docker, int.
4. **C4** `landlock_layer` handles no access right -> B5 fails. CI, int.
5. **C5** `next_rung` maps 71 to mount-only -> B6 fails. Docker, lib.
6. **C6** `admit` treats `Partial` as `Failed` -> B7 fails. Docker, lib.
7. **C7** the trampoline ignores `--allow` arguments beyond the policy's
   (the test's extra prefix) -> B3 fails. Docker, int.

Grouping: Docker int = C1, C3, C7; Docker lib = C5, C6; CI = C2, C4
(+ B8's acceptance run). B4 has no own control: C1-style "no layers"
fails it too, and the verify slice may add it to C1's group at no build
cost.

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
  Decided in section 3a (slice S6): the self-exec trampoline; no SDK
  change, so no further owner question.

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

### Slice S6 (design delta), after the rebase onto `release-4.14.0`

`select_project /Users/Valentin_Taiurskii/Projects/ClaudeProjects/g-mesh-wt-gm601`,
`project: "g-mesh-wt-gm601"` on each call; structural tier only
(`provenance: semanticTier absent`), direct call edges.

- `find_callers LspClient::start` (id `3e9bb2ca…`) -> `LspBridge::ensure_client`
  (`bridge.rs:706`) and the SDK test
  `the_server_runs_with_no_current_directory_exe_lookup_whatever_the_manifest_says`
  (`client.rs:916`); `hasMore: false`. Route (a) would touch both.
- `find_definition LspClient::start` -> `source.text`: one
  `Command::new(&config.command)` with args/env/`NO_CWD_EXE_LOOKUP`/cwd,
  no hook point today.
- `find_references SemanticConfig` (`answer: files`, id `07a23890…`) ->
  39 references in 14 files across `plugins/sdk`, `plugins/rust`,
  `plugins/python`, `plugins/typescript` (src and tests). Construction is
  via `from_manifest`/parse only (grep `SemanticConfig {` finds no struct
  literal outside `config.rs`), so a new field compiles everywhere, but it
  is an SDK interface every plugin sees: the blast radius of route (a).
- `find_definition SemanticConfig` -> derives `Debug, Clone, PartialEq, Eq`
  (`config.rs:233`): a closure field needs a hand-written wrapper.
- `find_references sandbox::confine` (id `4d7acb2c…`, after the name was
  ambiguous with `seatbelt::confine`) -> `semantic::server_config` and
  `sandbox::tests::confine_server`; `hasMore: false`. Adding `launcher` to
  `confine` touches exactly these two.
- grep (single known files, non-call-graph): `run.rs:115` (`std::env::args()`),
  `resolve.rs:148` (`probe` error text), the `"pub fn server_config("`
  needles in `semantic.rs` tests, `ci.yml` "Runner state".
