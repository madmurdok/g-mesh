# GM-681: a breadth guard on the sandbox's toolchain prefixes

Design note. Follows GM-674 (`GM-674-rustup-wrapper-sandbox.md`), whose
review finding F2 (4.21.0) is this task: the toolchain prefixes the
sandbox derives from binary locations have no upper bound on how broad they
may be.

## 1. Today

`SandboxPolicy::for_server` (`plugins/rust/src/sandbox.rs:93-123`) builds
the exec/map allowance `policy.toolchain` from six sources:

| # | Source | Derivation |
|---|---|---|
| S1 | sysroot | `rustc --print sysroot` (`Toolchain.sysroot`) |
| S2 | rustup dir | parent of the canonical first `rustup` on `PATH` (`Toolchain.rustup`, `semantic.rs:396-423`) |
| S3 | cargo proxy dir (GM-674) | parent of `canonicalize($CARGO_HOME/bin/cargo)` (`cargo_proxy`, `sandbox.rs:127-135`) |
| S4 | server dir | parent of the canonical rust-analyzer |
| S5 | rustc dir | parent of `Toolchain.rustc` |
| S6 | cargo dir | parent of `Toolchain.cargo` |

`SandboxPolicy::new` (`:75-90`) canonicalizes each one, drops the missing
ones and deduplicates (`canonical_prefixes`, `:137-148`). Nothing checks
how broad the result is. Every "parent of a binary" source is as broad as
wherever that binary happens to sit: a `rustup` (or a `cargo` proxy symlink
target) at `~/rustup` yields `$HOME`; a binary at `/rustup` yields `/`; a
rust-analyzer unpacked into `/opt` yields `/opt`.

Where the prefixes go (who consumes `policy.toolchain`):

- macOS seatbelt: `seatbelt::rules` (`sandbox.rs:241-290`) emits one
  `allow` of map+exec per prefix, ordered by path length; the project root
  deny and the writable-prefix exec denies still win over it.
- Linux: `landlock_ns::trampoline_args` (`sandbox/landlock_ns.rs:115-135`)
  passes each as `--allow`; the mount layer leaves it exec-able (every
  other mount is `noexec`) and Landlock grants exec beneath it. In the
  Landlock-only tier (`:185-200`) a root under an allowed prefix cannot be
  denied at all (`prefix_over`, `:331`): an allowed `/` or `$HOME` opens
  exec of the project's own binaries there.
- `semantic::admit` (`semantic.rs:263-300`) lists them in the start-up log.

So a too-broad prefix silently widens the confined tree's exec allowance to
the whole disk (`/`), every user file (`$HOME`, `/Users`), or a whole
system tree. Nothing is logged beyond the `admit` line's list.

Cross-file facts this note relies on, and the call that answered each:

| Question | Answer | g-mesh call |
|---|---|---|
| Who calls `for_server` | `semantic::server_config_with` (production) and 7 tests in `sandbox.rs` (`:511`, `:757`, `:797`, `:816`, `:843`, `:865`, `:887`) | `find_callers SandboxPolicy::for_server` |
| Who calls `cargo_proxy` | `semantic::script_proxy` (call) plus `for_server` (non-call ref) | `find_callers cargo_proxy` |
| Who calls `canonical_prefixes` | only `SandboxPolicy::new` | `find_callers canonical_prefixes` |
| Other policy builders | `SandboxPolicy::new` from `sandbox.rs` (6), `semantic.rs` tests (2, `:2436`, `:2455`, empty toolchain), `tests/semantic_runs_no_project_code.rs:2539` (empty toolchain) | `find_callers SandboxPolicy::new` (answer=files) |
| Consumers of the `toolchain` field | `seatbelt::rules:243`, `landlock_ns.rs:127, 195, 967`, `semantic.rs:275` | g-mesh does not index field reads (`find_references SandboxPolicy::toolchain` returned only semantic neighbours); answered by grep on the three files |

## 2. What "too broad" means

Each rule is checked against the **canonical** prefix (after
`canonicalize`), because that is what the sandbox matches; a symlink
`/opt/tools/bin/rustup -> /rustup` is judged as `/`.

| Rule | Refused shape | Examples |
|---|---|---|
| R1 root | a path with no normal component | `/`; Windows `C:\`, `\\?\C:\`, `\\server\share\` |
| R2 top level | exactly one normal component | `/usr`, `/opt`, `/etc`, `/var`, `/home`, `/Users`, `/Library`, `/System`, `/Applications`, `/nix`, `/private`; Windows `C:\Users`, `C:\Program Files` |
| R3 macOS aliases | `/private/<x>` | `/private/var`, `/private/tmp`, `/private/etc` (what `/var`, `/tmp`, `/etc` canonicalize to) |
| R4 volume and mount roots | `/Volumes/<x>`, `/System/Volumes/<x>`, `/mnt/<x>`, `/media/<x>`, `/media/<u>/<x>`, `/run/media`, `/run/media/<u>`, `/run/media/<u>/<x>` | `/Volumes/External`, `/System/Volumes/Data`, `/media/alice/usb`, `/run/media/alice` |
| R5 home | the canonical user home, or any ancestor of it | `/Users/alice`, `/home/alice`, `/home/users` for `HOME=/home/users/alice` |

Not refused, on purpose: `/usr/bin`, `/usr/lib`, `/usr/local`,
`/opt/homebrew`, `~/bin`, `~/.cargo/bin`, `~/.rustup/toolchains/...`. Every
depth-2 OS or package directory is already allowed by the backends
themselves (macOS `OS_EXEC`/`PACKAGE_PREFIXES`, Linux `OS_EXEC` holds all of
`/usr`), so refusing it as a toolchain prefix would change nothing; a
subdirectory of `$HOME` is where rustup itself lives.

R5's home is the same one `semantic::cargo_home` uses: `HOME`, else
`USERPROFILE`, non-empty, canonicalized; a home that does not exist
canonicalizes its nearest existing ancestor and appends the remaining
components (`HOME=/var/nohome/u` on macOS -> `/private/var/nohome/u`), so it
still compares against canonical prefixes; no home known means R5 is skipped
(R1-R4 still apply). R4 also covers `/run/media` and `/run/media/<u>`, the
parents of udisks2 mount roots, as `/media/<u>` covers those under `/media`.

## 3. Decisions

### D1. Refuse (drop) a too-broad prefix, do not narrow it (recommended)

- **Today:** a prefix of any breadth is allowed as is.
- **Change:** in `for_server`, after canonicalization, a prefix matching R1-R5
  is dropped from `policy.toolchain` and logged (D3). The other prefixes
  are unaffected.
- **Example:** rustup at `/Users/alice/rustup` -> S2 is `/Users/alice`
  (R5) -> dropped; S1 (`~/.rustup/toolchains/stable-.../`) and S3
  (`~/.cargo/bin`) stay, so rust-analyzer still runs the proxies and the
  toolchain.
- **Consequence:** the tree can never exec under a broad prefix. A toolchain
  binary that sits directly in such a directory is no longer exec-able; see
  section 4 for what that breaks.
- **Alternatives:**
  - *Narrow to the binary itself* (a literal-file rule: seatbelt
    `(literal ...)`, a Landlock rule on the file, a single-file bind in the
    mount layer). Benefit: a binary at `~/rust-analyzer` keeps working.
    Risk: a new rule kind in both backends and in the trampoline's argv
    (`--allow-file`), three code paths to test for one rare layout. A
    binary in `/` or `$HOME` directly is a layout to fix, not support.
  - *Narrow the sysroot to `<sysroot>/{bin,lib,libexec}`*, each re-checked.
    Benefit: a `install.sh --prefix=$HOME` toolchain (sysroot `$HOME`)
    keeps working. Risk: still allows `~/bin` and `~/lib` (user files);
    needs a per-source rule; the realistic broad sysroot (`/usr`, distro
    Rust on Linux) is already covered by the backend's `/usr`. Can be added
    later without changing D1.

### D2. Drop the one prefix, do not refuse the semantic tier

- **Today:** an unconfinable start (`SandboxOutcome::Failed`) is the only
  sandbox reason that refuses the tier (`admit`).
- **Change:** a dropped prefix is not a refusal; the server starts with the
  rest of the policy.
- **Example:** distro Rust on Debian: sysroot `/usr` (R2) dropped; the
  Linux backend allows `/usr` itself, so nothing changes for the user except
  one log line.
- **Consequence:** security is the same as a refusal (the broad prefix is
  never granted), and the common harmless cases (sysroot `/usr`, a stray
  rustup in `$HOME` beside a proper `~/.cargo/bin`) keep working. When the
  dropped prefix *was* needed, the failure is either loud (server dir: the
  preflight `--version` probe fails -> `Failed` -> tier refused with the
  reason) or quiet-but-logged (proxy dir: rust-analyzer's `cargo metadata`
  fails, as in GM-674, with D3's line naming why).
- **Alternative:** refuse the tier whenever any prefix is dropped. Benefit:
  never a half-working server. Risk: breaks distro Rust on Linux, where the
  drop changes nothing.

### D3. One log line per dropped prefix

- **Today:** only `admit`'s list of what *is* allowed.
- **Change:** `for_server` emits, per distinct dropped prefix:
  `[rust] semantic tier: sandbox: not allowing exec under <prefix>, the
  <source> (<reason>); rust-analyzer cannot run anything that lives only
  there`, where `<source>` is `sysroot`, `directory of rustup <path>`,
  `directory of the cargo proxy <path>`, `directory of rust-analyzer
  <path>`, `directory of rustc <path>`, `directory of cargo <path>`, and
  `<reason>` is `the filesystem root`, `a top-level directory`, `a system
  alias directory`, `a volume or mount root`, `your home directory or one
  above it`. A prefix dropped for two sources is logged once, naming the
  first.
- **Consequence:** the GM-674-style quiet failure gets a cause in the log.
- **Testability:** the message is built by a pure function over a
  `Refusal { prefix, source, reason }` list, so tests assert the list and
  the text without capturing logs.

### D4. The guard lives in `for_server`, not in `SandboxPolicy::new`

- **Today:** `new` takes any prefixes; tests build policies with
  `/usr/local` or literal structs to pin rule ordering.
- **Change:** the guard runs only on the six derived sources in
  `for_server`. `new`, the backends' own `OS_*`/`PACKAGE_PREFIXES`, and
  `policy.writable` are untouched (writable prefixes are already denied
  exec).
- **Consequence:** the ordering tests that use `/usr/local`
  (`sandbox.rs:446`, `:575`) need no change; a caller that hand-builds a
  policy is trusted, as today.

### D5. `for_server` takes the home as a parameter

- **Today:** `for_server(root, toolchain, server, cargo_home, temp)`.
- **Change:** add `home: Option<&Path>`; `server_config_with` passes the
  home that `cargo_home()` already computes (extract a `user_home()` from
  `semantic.rs:838-848` so both use one rule). The 7 test callers pass
  `None` unless they pin R5.
- **Consequence:** R5 is testable with a temp "home", without touching the
  process environment (other tests run in parallel in the same process).
- **Alternative:** read `HOME` inside the guard. Benefit: no signature
  change. Risk: R5 untestable without `set_var`, which races parallel tests.

## 4. Does rust-analyzer still work?

| Dropped source | Shape | Result |
|---|---|---|
| S1 sysroot `/usr` | distro Rust, Linux | works: Linux `OS_EXEC` holds `/usr`. (macOS cannot have it: SIP.) |
| S1 sysroot `$HOME` or `/opt` | `install.sh --prefix` into a broad dir | rustc in `<prefix>/bin` stays allowed only if that dir is not itself broad; `librustc_driver` in `<prefix>/lib` cannot be mapped (seatbelt map rule; Linux `noexec` mount) -> rustc fails, build scripts/proc macros/`--print cfg` fail; workspace loads partly. Logged. |
| S2 rustup dir | rustup at `~/rustup` or `/rustup` | works when the proxies rust-analyzer execs live elsewhere (S3 `~/.cargo/bin`); otherwise as S3 below |
| S3 cargo proxy dir | `~/.cargo/bin/cargo` -> `~/rustup` | `cargo metadata` gets EPERM: the GM-674 failure, now logged with the cause |
| S4 server dir | rust-analyzer at `~/rust-analyzer`, `/opt/rust-analyzer` | preflight probe fails -> `Failed` -> tier refused with the reason (loud) |
| S5/S6 rustc/cargo dir | non-rustup binaries directly in a broad dir | as S1 (`$HOME`) or S3: logged failure of the tools' runs |

Every row that breaks is a binary placed directly in `/`, a top-level dir,
a volume root or `$HOME`; the fix for the user is to move it one level
down. Covered by the log line (D3).

## 5. Platform differences

- **macOS:** canonical paths turn `/tmp`, `/var`, `/etc` into
  `/private/...` (R3); `/System/Volumes/Data` is the data volume's root
  (R4); `$HOME` is `/Users/<u>`, whose parent `/Users` is R2 anyway.
- **Linux:** the broad-prefix harm is largest: a Landlock grant has no deny
  inside it, so in the Landlock-only tier an allowed `/` or `$HOME` opens
  exec under the project root too. Mount roots `/mnt/x`, `/media/...`,
  `/run/media/u/x` (R4). `/home/u`, `/root` (R2) for `HOME`.
- **Windows:** no sandbox backend (`confine` -> `NoBackend`, server
  unconfined), so the guard only trims a policy nobody enforces; it is kept
  platform-neutral (path components, not strings) so the rules hold if a
  backend lands. R1 covers drive roots (`C:\`, verbatim `\\?\C:\`, UNC
  share roots); R2 covers `C:\Users`. Tests for these run under
  `#[cfg(windows)]`, since `C:\` is not a root on Unix.

## 6. Edit map

| # | Fn / item | File | Lines (1-based) | Change / read |
|---|---|---|---|---|
| 1 | `SandboxPolicy::for_server` | `plugins/rust/src/sandbox.rs` | 93-123 | change: tag each source, canonicalize, split by `too_broad`, log refusals (D3), pass the kept ones to `new`; add `home` (D5); doc comment |
| 2 | new `too_broad(prefix, home) -> Option<Reason>` | `plugins/rust/src/sandbox.rs` | after 135 (beside `cargo_proxy`) | add: R1-R5 over `Path::components` |
| 3 | new `Refusal` + `message()` | `plugins/rust/src/sandbox.rs` | beside 2 | add: D3 text, pure |
| 4 | `SandboxPolicy.toolchain` doc | `plugins/rust/src/sandbox.rs` | 40-44 | change: "never root, top-level, volume root or home-level" |
| 5 | `canonical_prefixes` | `plugins/rust/src/sandbox.rs` | 137-148 | read (reuse or mirror its canonicalize+dedup) |
| 6 | `cargo_home` -> `user_home` | `plugins/rust/src/semantic.rs` | 838-848 | change: extract `user_home()`, canonicalized; `cargo_home` uses it |
| 7 | `server_config_with` | `plugins/rust/src/semantic.rs` | 171-221 | change: pass `user_home().as_deref()` to `for_server` |
| 8 | 7 test callers of `for_server` | `plugins/rust/src/sandbox.rs` | 520, 757-880, 887-917 | change: add `None` argument |
| 9 | `seatbelt::rules` | `plugins/rust/src/sandbox.rs` | 241-290 | read only (consumer) |
| 10 | `trampoline_args`, Landlock-only gaps, `prefix_over` | `plugins/rust/src/sandbox/landlock_ns.rs` | 115-135, 185-200, 317-333 | read only (consumers) |
| 11 | `admit` | `plugins/rust/src/semantic.rs` | 263-300 | read only (log of kept prefixes) |
| 12 | ADR text | `docs/adr/0039-rust-server-runs-under-an-os-sandbox.md` | 34-36, 123-125 | change: toolchain prefixes never root, top-level, volume root or home-level; dropped ones are logged |

New tests go in the `sandbox.rs` tests module beside
`a_script_proxy_allows_its_own_directory_and_not_its_target` (`:866`).

## 7. Behaviours the tests must pin

Pure-function tests (`too_broad`, string paths, no filesystem), one per
refused shape plus the not-refused neighbours:

- B1 R1: `/` refused.
- B2 R2: `/usr`, `/opt`, `/Users`, `/home` refused.
- B3 R3: `/private/var` refused; `/private/var/folders/x/y/T/scratch` allowed.
- B4 R4: `/Volumes/X`, `/System/Volumes/Data`, `/mnt/x`, `/media/u/x`,
  `/run/media/u/x` refused; `/Volumes/X/rust/bin` allowed.
- B5 R5: with home `/h/u`: `/h/u` and `/h` refused, `/h/u/.cargo/bin`
  allowed; with home `None`, `/h/u` allowed.
- B6 neighbours allowed: `/usr/bin`, `/usr/local`, `/opt/homebrew`,
  `/nix/store/abc-rust/bin`.
- B7 Windows (`#[cfg(windows)]`): `C:\`, `\\?\C:\` refused (R1),
  `C:\Users` refused (R2), `C:\Users\u\.cargo\bin` allowed.

`for_server` tests (temp dirs, real canonicalization):

- B8 symlink judged by target: a temp home holding `bin/cargo` symlinked to
  a `rustup` placed directly in the temp "home" -> S3 resolves to the home,
  is dropped, and a `Refusal` names source "cargo proxy" and reason R5; the
  other prefixes stay.
- B9 R5 via `home`: `Toolchain.rustup` directly in the given home -> its
  dir dropped; with `home: None` the same toolchain keeps it.
- B10 log text: `Refusal::message()` names the prefix, the source with its
  binary path, and the reason.
- B11 dedup: the same broad prefix from two sources yields one `Refusal`.
- B12 no regression: the GM-674 tests
  (`for_server_allows_the_directory_of_the_proxy_cargo_home_execs` etc.)
  still pass with `home: None`, and B9's "kept" arm shows a
  `~/.cargo/bin`-shaped prefix below a given home is not dropped.

R1-R4 through `for_server` would need binaries in `/` or `/usr`; they are
pinned on the pure function only (B1-B4), plus B8/B9 proving `for_server`
applies the function.

## 8. Suggested controls (revert code, never the test)

All are lib tests of `g-mesh-plugin-rust` (`--lib`): one build per group.

- C1 (B1): drop R1 from `too_broad`. B1 fails.
- C2 (B2): drop R2. B2 fails.
- C3 (B3): drop R3. B3 fails.
- C4 (B4): drop R4. B4 fails.
- C5 (B5, B9): drop R5 (ignore `home`). B5, B9 fail.
- C6 (B8): judge the non-canonical prefix (run the guard before
  canonicalization). B8 fails (`$HOME/bin` is not broad; its target is).
- C7 (B8, B9): keep refused prefixes in `policy.toolchain` (compute
  refusals, do not filter). B8, B9 fail.
- C8 (B11): no dedup of refusals. B11 fails.

Eight controls, one per rule plus the two wiring points; C1-C5 can share
one worktree build sequence.

## 9. Must confirm

1. Owner: D1 (drop, no narrowing) over the literal-file or sysroot-narrowing
   alternatives.
2. Owner: D2 (drop the prefix, keep the tier) over refusing the tier.
3. Owner: the rule set R1-R5 as "too broad", in particular that depth-2 OS
   dirs (`/usr/bin`, `/usr/local`) stay allowed because the backends allow
   them anyway.
4. Owner, optional extension: also refuse the shared temp dir
   (`std::env::temp_dir()`, world-writable) and any ancestor of the project
   root (Landlock-only tier cannot deny the root under it). Both are in the
   spirit of F2 but not in its acceptance; tests place toolchains in
   subdirectories of the temp dir, so the first would not break them.
5. Implementer: `/System/Volumes/Data` is not the canonical form of any
   path a toolchain reports today (firmlinks keep `/Users/...`); R4 keeps
   it for a path given through the volume. Check `canonicalize` on a
   `/System/Volumes/Data/Users/<u>` path once on macOS 15.
6. Implementer: confirm on Linux CI that Debian's `rustc` (sysroot `/usr`)
   is the only CI image where a prefix is dropped, and that its log line
   appears once.

## 10. Risks

- A user with a binary directly in `$HOME` or a top-level dir loses a
  setup that worked before (quietly for S3/S5/S6, loudly for S4); the log
  line is the only signal. Mitigation: D3's wording names the binary.
- R5 depends on `HOME`/`USERPROFILE`; a process started with a wrong or
  empty `HOME` gets R1-R4 only.
- The static R4 list misses mount points elsewhere (`/srv/data`). A
  generic `st_dev` mount-point check was considered and rejected: on NixOS
  `/nix/store` and on containers `CARGO_HOME` volumes are mount points, so
  it would refuse legitimate parents and differs per machine, which makes
  tests non-deterministic.
- Signature change of `for_server` touches 8 call sites, all in two files.
