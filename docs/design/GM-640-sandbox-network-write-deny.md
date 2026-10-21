# GM-640: deny network and out-of-cache writes in the rust-analyzer OS sandbox

Status: design (slice S1), 2026-10-21. Follows GM-600 D4 A ("Только
загрузка/запуск, сеть — отдельной задачей") and GM-601 D4 A: this is that
separate task. Builds on [ADR 0039](../adr/0039-rust-server-runs-under-an-os-sandbox.md)
and makes the OS enforce what [ADR 0033](../adr/0033-semantic-tiers-reach-no-network-and-write-nothing.md)
enforces today only by configuration (`CARGO_NET_OFFLINE`, init options,
rust-analyzer's own `-Zlockfile-path`).

## Question

Under the sandbox, rust-analyzer and every process it starts must open no
network connection and write nothing outside the cargo caches it needs and
a temp directory. What exactly do rust-analyzer, cargo, rustc and rustup
write on a clean project; which Seatbelt rules and which Linux rules
(Landlock rights by ABI, the mount layer) express that; how the
Landlock-only and mount-only tiers degrade; and what fixture proves it.

## Experiments (2026-10-21)

macOS host: 26.6.2, x86_64, `stable-x86_64-apple-darwin`, rust-analyzer
1.97.1. `uptime` at the start: load 34.5 / 15.8 / 9.5 (other agents'
builds), at the end 4.4. Timings are indicative only; every arm waited on
rust-analyzer's `quiescent`, not a timer. `/usr/bin/time -p` of the driver:
`real` 15-46 s at `user` 0.05-0.11 s (the driver waits on the server).

Linux: Docker Desktop 28.3.2, kernel 6.10.14-linuxkit (no Landlock),
image `g-mesh-linux-sandbox-test:rust-1-bookworm` plus `strace`/`python3`,
rust-analyzer **1.99.1** (the image's `rust:1`, newer than CI's pin), uid
1000, `HOME=/home/u`, `CARGO_HOME=/usr/local/cargo`, `TMPDIR` unset,
`--network none --security-opt seccomp=unconfined`, `itoa` pre-fetched into
the registry volume.

Scratch: this session's `scratchpad/gm640/` (`lsp_drive.py` from GM-600
with an env-controlled stderr and `BUILD_SCRIPTS`, `mac*.sh`, `linux*.sh`,
`nsro.py`, `*.out`). Every arm used `pinned_env`'s variables and the
plugin's init options (`checkOnSave=false`, build scripts and proc macros
off, `cargo.sysroot` pinned) and asked `definition` of a cross-file call.
Fixtures: `fxa` = workspace `app` (`main` calls `util::Thing.method()`),
no `Cargo.lock`; `fxb` = the same plus `itoa = "1"` (in the local cache).

How writes were observed: on macOS a profile that denies every write except
`/dev` and reads the kernel's `Sandbox: <proc> deny(1) <op> <path>` lines
from `log show` (filtered to `rust-analyzer|cargo|rustc|rustup`), plus a
`find -newer` sweep; on Linux `strace -f` of every write-ish and socket
syscall, unconfined.

### What the server tree writes (clean fixtures, offline)

| Writer | Path | macOS | Linux | Needed? |
|---|---|---|---|---|
| rust-analyzer | `$TMPDIR/rust-analyzer<pid>-<n>/Cargo.lock` (its `-Zlockfile-path` target, one dir per `cargo metadata`, removed after) | yes (3-6 dirs) | yes (`/tmp/...`, 3 dirs) | **yes**: denied, rust-analyzer passes no lockfile path and cargo writes `<root>/Cargo.lock` instead (W2) |
| cargo | `$CARGO_HOME/.package-cache` (lock file) | yes | yes | no: denied, cargo runs on (EPERM on macOS, EROFS on Linux) |
| cargo | `$CARGO_HOME/.global-cache`, `.global-cache-journal` (last-use tracker) | yes | yes | no: same |
| cargo | `$CARGO_HOME/registry` (xattr, `CACHEDIR.TAG`, extraction into `registry/src`) | xattr only when `src` is extracted | `mkdir` (EEXIST) | **yes when a cached `.crate` is not extracted** (C1 vs C2) |
| cargo | `$CARGO_HOME/git/db/<name>/tmp` (git dependency) | only with a git dependency | - | **yes for a git dependency** (GS2) |
| rustc, rustup | - | nothing | nothing | - |
| any | `$DARWIN_USER_CACHE_DIR`, `~/Library/Caches`, `~/.cache`, `$RUSTUP_HOME` | nothing | nothing | - |
| any | network | no attempt logged | no `socket()` at all | - |

### Arms

| # | Arm | Result |
|---|---|---|
| U | `fxa`/`fxb` unconfined | health ok, resolved; writes as in the table, nothing under the root |
| D | GM-600 profile + `(deny file-write*)` (allow `/dev`) + `(deny network-outbound)` | health **warning**: `failed to write <root>/Cargo.lock ... Operation not permitted`; still resolved (rust-analyzer falls back to `--no-deps`). Denials: the table's rows plus `rust-analyzer deny file-write-create $TMPDIR/rust-analyzer…` |
| L | D + allow writes to `$TMPDIR`, `~/.cargo`, `~/.rustup`, root write-deny last | health ok, resolved, no denial |
| C0 | writes to `$TMPDIR` only (no `CARGO_HOME`) | health ok; `.global-cache`, `.package-cache`, `registry` xattr denied and tolerated |
| C1 | scratch `CARGO_HOME` holding the index and `itoa-1.0.18.crate` but no `registry/src`; writes to `$TMPDIR` + `CARGO_HOME/registry` | health ok, `itoa` extracted; root files denied and tolerated |
| C2 | C1 without `CARGO_HOME/registry` writable | health **warning** (metadata with dependencies failed: `registry/src` create denied); `itoa` not extracted |
| GU / GS | offline removed, git dependency at `http://127.0.0.1:<sink>/…`; unconfined / sandboxed with `CARGO_HOME/{registry,git}` writable | sink +3 connections / **+0**, `cargo deny(1) network-outbound remote:*:<port>` |
| GS2 | GS without any `CARGO_HOME` write | +0, but cargo failed earlier at `git/db/<name>/tmp` create: **no connect was attempted** (a network test needs `git/` writable to be discriminating) |
| NU / NS | offline removed, empty scratch `CARGO_HOME`, proxies to the sink | +3 / **+0** |
| W1 | build scripts **on** (configuration removed), `app/build.rs`; unconfined / sandboxed | `target/` + `CACHEDIR.TAG` written under the root / **nothing**, health warning "Failed to run build scripts", still resolved |
| M4 | private `TMPDIR` (`mktemp -d $TMPDIR/g-mesh-rust-XXXXXX`) writable, `CARGO_HOME/{registry,git}` writable, `(deny network*)` | health ok, resolved; only `.global-cache`/`.package-cache` denied; the private dir empty afterwards |
| P | lockless fixture, `app` calls `helper::assist()` in a path dependency `exclude`d from the workspace; private `TMPDIR` writable / not | `definition` -> `helper/src/lib.rs` / **`[]`** (the `--no-deps` fallback drops the non-member dependency) |
| HL | `(deny file-write*)`, `tmp` allowed, root denied; `ln root/f tmp/l`, `mv root/f tmp/m` | both `Operation not permitted`, `root/f` unchanged: a hard link or rename out of the root is a write of the root path |

Linux:

| # | Arm | Result |
|---|---|---|
| E1 | `nsro.py` (the trampoline's mount steps in Python): `unshare(USER\|NS\|NET)`, `/` private, `mount_setattr(/, RDONLY, AT_RECURSIVE)`, `/tmp/w` bound and `RDONLY` cleared, root `/tmp/w/proj` bound `RDONLY` | write `/tmp/w/ok` OK; `/tmp/w/proj/MARK`, `/home/u/out/MARK`, `/usr/local/cargo/MARK`, `mkdir root/target` **EROFS**; `/dev/null` writable (device nodes ignore a read-only mount); TCP to a listener on `127.0.0.1` **refused**; the namespace has only a down `lo` |
| E2 | E1 without `CLONE_NEWNET` | TCP to `127.0.0.1` **connected** (so E1's refusal is the network namespace) |
| E3 | rust-analyzer under E1's layer, `/tmp` and `CARGO_HOME` writable | resolved; root unchanged (the sysroot warning is the image's missing `rust-src`) |
| E4 | `/tmp` writable, `CARGO_HOME` read-only | resolved, no cargo warning |
| E5 | `CARGO_HOME` writable, `/tmp` read-only | health warning `failed to write <root>/Cargo.lock ... Read-only file system`; resolved via fallback |
| E6 | E3 + build scripts on + `build.rs` | `FetchBuildDataError: Read-only file system ... <root>/targetqR6gfk`; root unchanged; resolved |

Load-bearing facts:

1. **The temp directory is the one write the tier cannot lose.** Without
   it rust-analyzer passes no lockfile path, cargo tries `<root>/Cargo.lock`,
   the sandbox refuses it, `cargo metadata` with dependencies fails and every
   dependency edge is lost (P). With it, a lockless project writes nothing
   under its root, whatever rust-analyzer version (the lockfile now lands
   in the temp dir or nowhere): ADR 0033's "an older rust-analyzer may
   write `Cargo.lock`" becomes an OS refusal.
2. **Of `CARGO_HOME`, only `registry/` and `git/` are needed**, and only for
   extraction of a cached crate and a git dependency's checkout. cargo
   tolerates the refused lock and tracker files on both OSes.
3. **The Linux network namespace is what blocks UDP and loopback in the
   mount-only tier**; Landlock (ABI >= 4) only ever covered TCP.

## 1. Policy: what may be written, what may connect

`SandboxPolicy` gains `writable: Vec<PathBuf>` (canonical, existing,
deduplicated, like `toolchain`). `for_server` fills it with:

| Prefix | Source | Why |
|---|---|---|
| the server's private temp dir (D1) | created by `server_config_with`, set as `TMPDIR` in `config.env` | rust-analyzer's lockfile dirs (fact 1) |
| `$CARGO_HOME/registry` | `semantic::cargo_home()` (env `CARGO_HOME`, else `~/.cargo`) | extraction of a cached `.crate` (C1/C2) |
| `$CARGO_HOME/git` | same | a cached git dependency's checkout (GS2) |

Each is dropped when it does not exist (a missing `registry/` means
nothing is cached, so nothing can be resolved offline and nothing needs
writing). Never writable: the project root (even inside a writable
prefix), `$CARGO_HOME` itself and its `bin/`, `config.toml`,
`credentials.toml`, `env` (a write there would persist into the user's own
later cargo runs, outside any sandbox), `$RUSTUP_HOME`, the user's shared
temp dir (D1), `$HOME`. OS write prefixes are each backend's own (`/dev`).

Ordering is the existing rule: a writable prefix inside the root (indexing
`$HOME`, which holds `~/.cargo/registry`) stays writable; the root inside a
writable prefix stays read-only. Network: nothing may connect, bind or
listen; rust-analyzer talks over stdio only.

`for_server(root, toolchain, server)` becomes
`for_server(root, toolchain, server, cargo_home: Option<&Path>, temp: &Path)`
so it stays a pure function of its arguments (the unit tests pass a fake
`CARGO_HOME`; no test may change the process environment).

## 2. Seatbelt (macOS)

Profile; the new lines are marked `new`:

```scheme
(version 1)
(allow default)
(deny file-map-executable)
(deny process-exec)
(deny file-write*)                                         ; new: default deny
(deny network*)                                            ; new: outbound, inbound, bind
;; ascending path length, last match wins (GM-600 E8):
(allow file-map-executable process-exec (subpath (param "ALLOW_n")))   ; toolchain
(allow file-write* (subpath (param "ALLOW_m")))            ; new: policy.writable
(deny  file-map-executable process-exec file-write* (subpath (param "ROOT")))  ; root deny gains file-write*
(allow file-map-executable (subpath "/System") …)          ; OS_MAP
(allow process-exec (subpath "/bin") …)                    ; OS_EXEC
(allow file-write* (subpath "/dev"))                       ; new: OS_WRITE (/dev/null, /dev/tty, /dev/dtracehelper)
```

- `rules()` gets the writable prefixes as `(true, WRITE, path)` entries and
  the root deny's `ops` becomes `MAP_EXEC_WRITE`; sorting, dedup and
  parameter naming are unchanged, so write rules take part in the same
  length ordering (a write allow inside the root is emitted after the root
  deny and wins).
- `(deny network*)` rather than `network-outbound`: measured working (M4),
  and it also refuses `bind`/`listen`, which nothing in the tree needs.
  Mach services are not filtered (denying `mach-lookup` breaks dyld and
  libsystem); none of the tree's binaries asks a system daemon to fetch.
- `file-write*` covers create, data, unlink, rename, mode, xattr, times;
  a hard link or rename out of the root is refused (HL).
- Preflight: unchanged (`--version` of the wrapped server proves the
  profile parses with every parameter bound).

## 3. Linux

### Mount layer (trampoline, `mount_layer`)

1. `unshare(CLONE_NEWUSER | CLONE_NEWNS | CLONE_NEWNET)` (D3): the new
   network namespace has only a down `lo`, so every TCP, UDP and abstract
   unix socket connect fails, loopback included (E1/E2). Pathname unix
   sockets stay reachable (a filesystem object; see the Landlock layer).
2. uid/gid maps, `/` private: unchanged.
3. `mount_setattr("/", AT_RECURSIVE, attr_set = NOEXEC | RDONLY)`: default
   deny for both.
4. For each step of `mount_plan`, ascending path length, the root last at
   equal length: bind `P` on itself, then set/clear `NOEXEC` and `RDONLY`
   for that path as the most specific rule says. A path's two flags are
   decided independently: exec from the exec prefixes and the root, write
   from the writable prefixes and the root.

`mount_plan(root, exec, write) -> Vec<MountStep { path, exec: bool, write: bool }>`
stays pure. Device nodes ignore a read-only mount (`/dev/null` stays
writable, E1), so `/dev` needs no step. A prefix inherited already
read-only from the parent namespace stays locked (`MNT_LOCK_READONLY`),
which is correct.

The capability drop (unchanged) is what keeps a uid-0 server from
`mount -o remount,rw` of the root, as it already keeps it from `exec`.

### Landlock layer (`landlock_layer`)

- `handled_access_fs = Execute | AccessFs::from_write(ABI::V9)`, best
  effort (the crate downgrades to the kernel's ABI): `WriteFile`,
  `Remove*`, `Make*` (ABI 1), `Refer` (2), `Truncate` (3), `IoctlDev` (5),
  `ResolveUnix` (9, connect to a pathname unix socket).
- Rules: `Execute` beneath the exec prefixes (unchanged);
  `from_write(V9)` beneath every writable prefix and beneath `/dev`.
- Net and scope: unchanged (`BindTcp | ConnectTcp`, no rules, ABI >= 4;
  abstract unix sockets and signals scoped, ABI >= 6).

Landlock has no deny inside a grant: a root inside a writable prefix would
be writable in the Landlock-only tier. With D1 A the writable prefixes are
a fresh private dir and two `CARGO_HOME` subdirs, so a root lies inside
one only by construction of the user (a project inside
`~/.cargo/registry/src`); `confine` names it in the gap, as it already
does for exec (`prefix_over`).

### Trampoline argv

`--write <prefix>` (repeatable), after the `--allow` list. `parse` and
`trampoline_args` gain it; `Invocation` gains `write: Vec<PathBuf>`. No new
exit code: a failing `CLONE_NEWNET`, `RDONLY` set or rw bind is a mount
layer failure (71), a failing write rule a Landlock failure (72).

### Tiers

| Tier | Exec / map | Writes | Network |
|---|---|---|---|
| full: mounts + Landlock | blocked | blocked (read-only mounts and Landlock) | all blocked (namespace); TCP also by Landlock >= 4; pathname unix sockets by Landlock >= 9 |
| mount-only (no Landlock) | blocked | blocked (read-only mounts) | all but pathname unix sockets (namespace) |
| Landlock-only (`Partial`, no user namespace) | exec only (GM-601 D2) | blocked at every ABI, except `truncate` of an existing file below ABI 3, and anything under a root inside a writable prefix | TCP at ABI >= 4; **UDP never**; abstract unix at >= 6; pathname unix at >= 9 |
| neither | `Failed`, structural tier | | |

`Confined` stays `Confined` for the mount-only tier: the namespace covers
everything the policy asks except pathname unix sockets, which the Seatbelt
backend covers and Landlock covers from ABI 9. The Landlock-only tier keeps
GM-601 D2's admission; its `gap` grows a network and write clause built by
a pure `landlock_gaps(abi: i64, root, writable) -> Vec<String>`
("UDP and unix-socket connects are not blocked", "TCP is not blocked
(Landlock ABI <n> < 4)", "truncation is not blocked (ABI < 3)", "writes
under the root are not blocked: it lies under the writable prefix <p>").
The ABI is read in the parent with the `landlock_create_ruleset(NULL, 0,
VERSION)` syscall the tests already use (`linux_sandbox::landlock_abi`),
moved into `landlock_ns` as production code.

Backend strings (`"noexec mounts + Landlock"`, `"noexec mounts"`) stay, so
no assertion on them changes; `admit`'s `Confined` line gains "it may
write only <writable>, and opens no network connection".

## 4. The private temp dir (D1 A)

`server_config_with`, after `pinned_env`: on macOS and Linux create
`std::env::temp_dir()/g-mesh-rust-<pid>-<n>` (mode 0700, `n` a process
counter, so a second engine in one process gets its own), insert
`TMPDIR=<dir>` into `config.env`, pass it to `for_server`. rust-analyzer
and cargo read `TMPDIR` through `std::env::temp_dir`. rust-analyzer removes
its own subdirs (M4: empty afterwards); the empty dir itself is left for
the OS's temp cleaning (macOS purges `/var/folders/*/T` after 3 days, Linux
`/tmp` at reboot or by `systemd-tmpfiles`). On an OS with no backend the
dir is not created.

## 5. Fixtures and markers

The markers follow GM-600/601: a file that must not appear and a listener
that must see no connection, each with an unconfined sanity arm proving the
attempt is live. A clean-fixture assertion (`resolved_method`, or the path
dependency) proves the tier still works. Every integration test builds the
config through `semantic::server_config_with` (bypassing the cargo-config
pre-check, as GM-600/601 do) and then **removes the configuration that
holds ADR 0033 today, test-side**, so only the sandbox stands between the
tree and the marker. No production switch.

### Integration: `plugins/rust/tests/semantic_runs_no_project_code.rs`, new `mod sandbox_writes_and_network`

`#[cfg(any(target_os = "macos", target_os = "linux"))]`, parent/child pairs
through `in_its_env` (the child test process keeps the process environment
untouched); on Linux each parent first calls
`linux_sandbox::host_runs_the_mount_layer` (skips where it cannot run,
panics on GitHub Actions, as today).

- **N - network.** Parent: scratch `CARGO_HOME` with empty `git/` and
  `registry/` (GS2: without a writable `git/` cargo fails before any
  connect, and the test would pass vacuously), passed to the child through a
  new `in_its_env_with(test, scratch, real, cwd, env: &[(&str, &OsStr)])`
  (`in_its_env` calls it with `&[]`). Child: starts a TCP listener that
  logs each accepted connection, writes the fixture with
  `gm640dep = { git = "http://127.0.0.1:<port>/gm640dep.git" }`; sanity:
  plain `cargo metadata` (no `CARGO_NET_OFFLINE`) in a copy reaches the
  listener; then `server_config_with`, **remove `CARGO_NET_OFFLINE`** from
  `config.env`, add `CARGO_NET_RETRY=0`, answer. Assert: no connection,
  `resolved_method`.
- **W - writes.** Fixture with `app/build.rs`, no `Cargo.lock`. Child:
  `server_config_with`, then test-side **`cargo.buildScripts.enable = true`**,
  `CARGO_TARGET_DIR=<CARGO_TARGET_TMPDIR>/gm640-<pid>/target` (outside the
  temp dir and the caches) and **`TMPDIR` pointed at an existing directory
  the policy does not list** (so rust-analyzer cannot place its lockfile
  and cargo tries `<root>/Cargo.lock`, the old-rust-analyzer case). Sanity:
  unconfined `cargo check` in a copy writes `target/`. Assert after the
  answer: the root's snapshot unchanged (no `Cargo.lock`, no `target*`),
  the outside target dir absent, `resolved_method`.
- **P - clean fixture.** Lockless workspace, `app` calls `helper::assist()`
  in a path dependency `exclude`d from the workspace (fixture P). Child:
  `server_config_with` unchanged (assert `config.env["TMPDIR"]` is a
  directory in the policy's writable list, i.e. not the user's temp dir),
  answer. Assert: an edge from `main` to `assist` (only `cargo metadata`
  with dependencies yields it; it needs the temp dir, fact 1).

### Unit, macOS (`plugins/rust/src/sandbox.rs` tests, `--lib`)

A `probe_server(dir)` beside `fake_server`: a `/bin/sh` script that
answers `--version` and otherwise runs `probe <files…> -- <port>`: writes
each file, then `/bin/bash -c 'exec 3<>/dev/tcp/127.0.0.1/<port>'`. Run as
`Command::new(config.command).args(config.args).args(probe…)` after
`confine`.

- **U1** `seatbelt_writes_only_under_writable_prefixes_and_never_under_the_root`:
  policy root `w/proj` inside writable `w`, toolchain `tool`; the probe
  writes `w/ok` (must exist), `w/proj/MARK` and `out/MARK` (must not),
  `/dev/null` (must succeed).
- **U2** `seatbelt_refuses_every_network_connection`: a listener; the
  probe run unconfined connects (sanity); confined, no accept within 2 s.
- **U3** extend `the_root_deny_follows_the_prefixes_that_hold_it_and_precedes_those_it_holds`
  with a writable prefix holding the root and one the root holds; the root
  rule's ops contain `file-write*`.
- **U4** `for_server_allows_writes_only_to_the_temp_dir_and_cargo_caches`:
  fake `CARGO_HOME` with `registry/`, `git/`, `bin/`; `writable` is exactly
  `[temp, registry, git]` (canonical), never `CARGO_HOME` or `bin/`; a
  missing `git/` is dropped.

### Linux (`linux_sandbox` module, trampoline level)

The `trampoline()` helper gains `write: &[&Path]` (every call site passes
`&[]` except below).

- **L1** `the_mount_layer_writes_only_under_write_prefixes_and_never_under_the_root`
  (Docker): `--write w`, root `w/proj`; `sh -c` writes `w/ok` (exists),
  `w/proj/MARK`, `out/MARK` (absent), `/dev/null` (succeeds).
- **L2** `the_mount_layer_reaches_no_network` (Docker): a TCP listener and
  a UDP socket on `127.0.0.1`; unconfined `bash /dev/tcp` and
  `/dev/udp` reach them (sanity); under `--layers mount` neither does.
- **L3** `landlock_alone_denies_writes_outside_the_write_prefixes` (CI,
  needs Landlock): `--layers landlock --write w`; `w/ok` written,
  `out/MARK` not.
- Fix **`the_mount_layer_maps_no_shared_object_under_the_root`**: its
  allowed object writes `tool_mark` into the root from inside the sandbox,
  which is now refused; move `tool_mark` into a `--write` dir (the
  GM-601 note foresaw this, section 5).
- Unit (`landlock_ns` tests, `--lib`): `mount_plan` with both flags (root
  inside a writable prefix is read-only; a writable prefix inside the root
  is writable; exec and write decided independently); `parse` /
  `trampoline_args` with `--write`; `landlock_gaps` per ABI.

## 6. Edit map

Line numbers 1-based (`get_file_outline` 0-based + 1), this worktree at
`54ad9d9`.

Change:

- `plugins/rust/src/sandbox.rs`
  - module doc (`1-18`): the sandbox now also decides writes and network.
  - `SandboxPolicy` (`31-38`): add `pub writable: Vec<PathBuf>` with doc.
  - `SandboxPolicy::new` (`67-78`): take `writable` too (canonicalize,
    drop missing, dedup); callers: `for_server`, the tests' policies
    (`sandbox.rs` tests `359-469`, `semantic_runs_no_project_code.rs`
    `linux_sandbox::confine_picks_the_tier_this_host_supports` `1827-`).
  - `for_server` (`84-95`): new `cargo_home`, `temp` parameters; writable =
    `[temp, cargo_home/registry, cargo_home/git]`.
  - `seatbelt` consts (`142-153`): `WRITE = "file-write*"`,
    `MAP_EXEC_WRITE`, `OS_WRITE = ["/dev"]`.
  - `seatbelt::rules` (`174-211`): writable allows, `OS_WRITE` allows, root
    deny with `MAP_EXEC_WRITE`.
  - `seatbelt::profile` (`215-223`): header gains `(deny file-write*)` and
    `(deny network*)`.
- `plugins/rust/src/sandbox/landlock_ns.rs`
  - module doc (`1-23`): read-only mounts, network namespace, write rights.
  - `trampoline_args` (`102-119`), `Invocation` (`203-209`), `parse`
    (`213-246`): `--write`.
  - `confine` (`143-199`), `Layers::Landlock` arm: gap from
    `landlock_gaps`; new pure `landlock_gaps` and production `landlock_abi`.
  - `trampoline` (`255-290`): pass `invocation.write` to both layers.
  - `mount_plan` (`316-321`): `(root, exec, write) -> Vec<MountStep>`.
  - `set_noexec` (`333-355`): generalize to `set_attrs(path, set, clr)`.
  - `mount_layer` (`360-400`): `CLONE_NEWNET`; default `NOEXEC | RDONLY`;
    per-step flags.
  - `landlock_layer` (`406-428`): handle `from_write(ABI::V9)`; write rules
    beneath writable prefixes and `/dev`.
- `plugins/rust/src/semantic.rs`
  - `server_config` doc (`141-160`): writes and network.
  - `server_config_with` (`168-200`): create the private temp dir, set
    `TMPDIR` in `config.env`, pass `cargo_home()` and the dir to
    `for_server`.
  - `admit` (`206-236`): the `Confined` line names the writable prefixes
    and "no network".
  - `pinned_env` doc (`551-575`): `CARGO_NET_OFFLINE` is now also enforced
    by the sandbox.
- `plugins/rust/tests/semantic_runs_no_project_code.rs`
  - `in_its_env` (`200-226`): split into `in_its_env_with(…, env)`.
  - new `mod sandbox_writes_and_network` (N, W, P).
  - `linux_sandbox` (`1535-`): `trampoline` helper (`1556-1564`) gains
    `write`; L1, L2, L3; fix `the_mount_layer_maps_no_shared_object_under_the_root`
    (`1693-1734`); `landlock_abi` (`1603-1617`) calls the production one.
- `scripts/test-linux-sandbox-docker.sh`: `int_filter` gains
  `sandbox_writes_and_network::`.
- `plugins/rust/plugin.toml` comment above `[plugin.semantic.initialization_options]`:
  offline and "writes no `target/`" are also OS-enforced.
- Docs: new ADR 0040 "the rust server's sandbox denies network and writes
  outside its temp dir and the cargo caches" (records D1-D3, supersedes ADR
  0039 D4 A and its "No Linux layer confines writes" consequence); ADR 0033
  Consequences: the `Cargo.lock` sentence gains the OS refusal; ADR 0039
  Status gains a pointer.

Read (no change): `Toolchain::detect` (`semantic.rs` `333-359`),
`cargo_home` (`776-784`, today called only by `cargo_env_loader_var`),
`engine_with` (`123-140`, unchanged: it calls `server_config_with`),
`project_extra_env::resolved_method` (`tests` `725-`),
`indexing_writes_nothing_and_stays_offline.rs` (stays green unchanged: its
empty `CARGO_HOME` has no `registry/`, so nothing is writable there, and
offline cargo has nothing to resolve).

## 7. Behaviours to pin

- B1 The confined tree opens no network connection, offline configuration
  removed (N; U2; L2). Ctl-2, Ctl-4.
- B2 The confined tree writes nothing outside the writable prefixes (W;
  U1; L1; L3). Ctl-1, Ctl-5, Ctl-7.
- B3 The root stays read-only inside a writable prefix; a writable prefix
  inside the root stays writable (U1, U3; `mount_plan` units). Ctl-3,
  Ctl-8.
- B4 The private temp dir is the server's `TMPDIR` and is writable, so a
  lockless project keeps its dependencies (P). Ctl-6.
- B5 The writable set is the temp dir and `CARGO_HOME/{registry,git}`,
  never `CARGO_HOME` itself (U4). No control (pure; its control would
  revert the line the assertion reads).
- B6 The semantic tier still works on clean fixtures: every existing
  sandboxed test (GM-600/601 tests, `indexing_writes_nothing_and_stays_offline`,
  `conformance`, `workspace_changed`) stays green.
- B7 The Landlock-only gap names the network and write holes per ABI
  (`landlock_gaps` units). No control (pure text).

## 8. Controls

Eight, reverting production code in a throwaway worktree. Group A builds
on macOS (one `--lib` build + `semantic_runs_no_project_code`); group B in
Docker (`scripts/test-linux-sandbox-docker.sh` route, uid 1000,
`seccomp=unconfined`); group C on CI (`workflow_dispatch`, GM-601 D5; needs
the owner's push approval).

| # | Revert | Must fail | Binary | Group |
|---|---|---|---|---|
| Ctl-1 | drop `(deny file-write*)` from `seatbelt::profile` | U1 (`out/MARK` written); W (outside target dir created) | `--lib`; `semantic_runs_no_project_code` | A |
| Ctl-2 | drop `(deny network*)` from `seatbelt::profile` | U2 (accepted); N (listener saw a connection) | `--lib`; `semantic_runs_no_project_code` | A |
| Ctl-3 | root deny back to `MAP_EXEC` in `seatbelt::rules` | U1 (`w/proj/MARK` written) | `--lib` | A |
| Ctl-6 | `server_config_with` does not set `TMPDIR` in `config.env` | P (`assist` unresolved) | `semantic_runs_no_project_code` | A (and B) |
| Ctl-4 | drop `CLONE_NEWNET` from `mount_layer`'s `unshare` | L2; N (Docker: mount-only tier, no Landlock) | `semantic_runs_no_project_code` | B |
| Ctl-5 | drop `RDONLY` from the default `/` `set_attrs` | L1 (`out/MARK` written); W | `semantic_runs_no_project_code` | B |
| Ctl-8 | `mount_plan` sorts the root first (no "most specific last") | `mount_plan` unit (root inside a writable prefix) | `--lib` | B |
| Ctl-7 | drop `from_write` handling in `landlock_layer` | L3 (`out/MARK` written) | `semantic_runs_no_project_code` | C |

## 9. Decisions for the owner

### D1 - which temp directory the server may write (recommended: A)

- **Today:** rust-analyzer writes into the user's temp dir
  (`$TMPDIR`, `/var/folders/<…>/T` on macOS, `/tmp` on Linux): one
  `rust-analyzer<pid>-<n>/Cargo.lock` per `cargo metadata`. Nothing stops
  any process of the tree writing anywhere else.
- **Change:** the temp dir becomes one of only three writable places
  (fact 1: without it a lockless project loses every dependency edge).
- **Example:** on Ubuntu 24.04 (Landlock-only tier, no user namespace) a
  user indexes a project cloned into `/tmp/proj`. With the shared `/tmp`
  writable, Landlock cannot carve `/tmp/proj` out of its grant, so the tree
  could write into that project; with a private `/tmp/g-mesh-rust-<pid>-0`
  it cannot.
- **A. A private dir per engine, `TMPDIR` set to it (recommended).**
  Benefit: the writable set no longer depends on where projects live; no
  root ever lies inside it, closing the Landlock-only hole above and making
  the tests' scratch roots (all under the temp dir) ordinary denied paths;
  shared `/tmp` stays untouched. Risk: one more environment override and a
  directory g-mesh creates; an empty `g-mesh-rust-*` dir per plugin process
  stays until the OS cleans the temp dir.
- **B. The user's temp dir as is.** Benefit: no new code beyond the rule.
  Risk: on Linux all of shared `/tmp` is writable to the tree; in the
  Landlock-only tier a project under `/tmp` is writable (logged as a gap).

### D2 - how much of `CARGO_HOME` the server may write (recommended: A)

- **Today:** cargo writes `$CARGO_HOME/.package-cache`, `.global-cache`
  on every `cargo metadata`, and `registry/` / `git/` when it extracts a
  cached crate or checks out a cached git dependency.
- **Change:** choose the writable part.
- **Example:** cargo's automatic cache cleaning removed
  `~/.cargo/registry/src/…/serde-1.0.200` after a month unused but kept the
  `.crate`; the next index of a project using it must extract it again. If
  `registry/` is not writable, `cargo metadata` fails and all dependency
  edges of that project are lost (C2).
- **A. `registry/` and `git/` only (recommended).** Benefit: extraction and
  git checkouts work (C1, GS); `config.toml`, `bin/` (rustup proxies, which
  the sandbox lets the tree exec) and credentials stay read-only, so nothing
  can persist into the user's own later cargo runs. Risk: cargo's lock file
  and last-use tracker are not updated for g-mesh's reads (cargo runs on
  without them, C0/E4); the cleaner may judge a crate unused that only
  g-mesh read. A write into `registry/src` (a dependency's sources) stays
  possible.
- **B. All of `CARGO_HOME`.** Benefit: cargo behaves exactly as
  unconfined. Risk: the tree can write `~/.cargo/config.toml` or a binary
  into `~/.cargo/bin`, which the user's next cargo run reads or execs
  outside any sandbox.
- **C. None.** Benefit: the strongest rule. Risk: every crate not already
  extracted, and every git dependency, goes unresolved (C2, GS2).

### D3 - how Linux blocks the network (recommended: A)

- **Today:** only Landlock's TCP rights (kernel ABI >= 4) block the
  network, so the mount-only tier (no Landlock) blocks nothing, and UDP
  (DNS) is never blocked.
- **Change:** add a network namespace to the mount layer.
- **Example:** on a kernel without Landlock, or a future cargo that
  resolves through UDP (DNS-based discovery), the server today could still
  send packets; inside an empty network namespace it has no interface to
  send them on (E1: loopback refused; E2: connected without it).
- **A. `CLONE_NEWNET` in the mount layer's `unshare`, Landlock TCP kept
  (recommended).** Benefit: one flag; the full and mount-only tiers block
  TCP, UDP and abstract unix sockets on any kernel; measured in Docker.
  Risk: a host where a user + mount namespace works but a network
  namespace does not would fall to the Landlock-only tier (no such
  configuration known: inside its own user namespace the process holds
  `CAP_SYS_ADMIN` for both).
- **B. Landlock only, as today.** Benefit: no change to the mount layer.
  Risk: the mount-only tier stays unblocked for every protocol, and UDP
  stays open in every tier; ADR 0033's "no network" is OS-enforced only for
  TCP on recent kernels.
- **C. A seccomp filter refusing `socket(AF_INET*)`.** Benefit: covers the
  Landlock-only tier's UDP gap too. Risk: a new mechanism and dependency in
  the trampoline, and a filter that must also allow `AF_UNIX` (stdio pipes
  aside, nothing needs it); more code than the gap justifies.

## Appendix: g-mesh calls used

Project `g-mesh-wt-gm640` (this worktree); the index built on first call.

- `select_project` -> served this worktree (later calls passed `project`:
  other agents shared the connection, and a call without it was refused
  with "more than one project was selected").
- `get_file_outline plugins/rust/src/sandbox.rs`,
  `plugins/rust/src/sandbox/landlock_ns.rs`, `plugins/rust/src/semantic.rs`
  -> every line range in the edit map.
- `find_callers sandbox::confine` -> `semantic::server_config_with`,
  `sandbox::tests::confine_server`,
  `linux_sandbox::confine_picks_the_tier_this_host_supports` (complete,
  `provenance: semanticTier absent`; `confine` is a free function, so no
  receiver call can be missing).
- `find_references SandboxOutcome` -> `sandbox.rs`, `landlock_ns.rs`,
  `semantic.rs` (`admit`), the integration test file: no new variant, so
  no `match` changes.
- `find_references SandboxPolicy` -> `new`, `for_server`, `confine`,
  `seatbelt::{rules, wrapper_args, confine}`, `landlock_ns::{trampoline_args, confine}`,
  `semantic::admit`, the tests (the new field's construction sites).
- `find_callers mount_plan` -> `mount_layer` only.
- `find_callers seatbelt::rules` -> `wrapper_args` and the ordering test.
- `find_callers cargo_home` -> `cargo_env_loader_var` only (reused by
  `server_config_with`).

grep was used for the integration test file's layout and for docs, the
Docker script and the CI workflow (non-code or one known file).

## Owner decisions (2026-10-21)

- D1: "Свой каталог на движок (Recommended)" — option A, a private
  `TMPDIR` per engine.
- D2: "Только registry/ и git/ (Recommended)" — option A.
- D3: "Сетевой namespace + Landlock TCP (Recommended)" — option A,
  `CLONE_NEWNET` in the mount layer, Landlock TCP kept.
