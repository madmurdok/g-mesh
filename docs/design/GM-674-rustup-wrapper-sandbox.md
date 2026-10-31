# GM-674: a rustup outside the sandbox's exec allowance

Design note. Follows GM-673 (`GM-673-aarch64-cargo-as-rustup.md`), whose
S4 saw the confined rust-analyzer get EPERM with a fake Homebrew keg first
on `PATH`, and whose S6 found a gap in the test proxies' fallback.

## 1. Today

The exec allowance of the confined tree (`plugins/rust/src/sandbox.rs`):

- `SandboxPolicy::for_server` (`:100-123`) builds `policy.toolchain` from
  the sysroot, the parent of `Toolchain.rustup`, and the parents of the
  server, `rustc` and `cargo`, each directory canonicalized by
  `canonical_prefixes` (`:126`).
- `Toolchain.rustup` is the first `rustup` on `PATH`, canonicalized
  (`Toolchain::detect`, `semantic.rs:389`). For Homebrew that is the keg's
  wrapper **script** (`/opt/homebrew/Cellar/rustup/<v>/bin/rustup`), not the
  binary it `exec`s (`.../libexec/bin/rustup`).
- The seatbelt backend adds `PACKAGE_PREFIXES` (`/opt/homebrew`,
  `/usr/local`, `/opt/local`, `/nix/store`) in `seatbelt::rules` (`:229`).
  The Linux backend (`sandbox/landlock_ns.rs:127`, `:195`, `:967`) reads
  `policy.toolchain` and has no package prefixes.

## 2. The problem, restated from evidence

The task says "a wrapper whose target lies outside PACKAGE_PREFIXES cannot
exec its toolchain". The S4 log (`gm673-s4-log.txt:96-108`) shows what was
actually denied: rust-analyzer ran

```
RUSTUP_AUTO_INSTALL="0" "/Users/<me>/.cargo/bin/cargo" "locate-project" ...
RUSTUP_AUTO_INSTALL="0" "/Users/<me>/.cargo/bin/cargo" "rustc" "-Z" "unstable-options" "--print" "cfg" ...
```

an **absolute `$CARGO_HOME/bin/cargo`**, not the `rustup` on `PATH`, not a
`PATH` lookup, and not `CARGO` (which `pinned_env` sets to the toolchain's
own cargo). This is rust-analyzer's own rule: with a sysroot set, its
`Sysroot::tool` runs `Tool::prefer_proxy()`, the rustup proxy in
`$CARGO_HOME/bin/<tool>` when that file exists (rustc excepted: it is taken
from `<sysroot>/bin`), else the usual lookup (`CARGO`, then `PATH`).

So the tree execs a rustup proxy at a path the plugin can compute exactly,
and the policy allows it only by coincidence: when `$CARGO_HOME/bin` is
also the directory of the first `rustup` on `PATH` (rustup-init), or lies
under a package prefix. In the fake keg the wrapper's target happened to be
`~/.cargo/bin`, which made it look like a wrapper-target problem.

Who is affected, by install shape:

| Install | `$CARGO_HOME/bin/cargo` exists | Tree execs | Allowed today |
|---|---|---|---|
| rustup-init only | yes (`~/.cargo/bin` = rustup's dir) | `~/.cargo/bin/cargo` -> rustup | yes (rustup dir) |
| Homebrew keg only (CI macos-15-arm64) | no | `CARGO` (toolchain cargo) | yes (cargo dir) |
| Homebrew keg first on `PATH` **plus** a rustup-init `~/.cargo/bin` (an earlier install kept, or both) | yes | `~/.cargo/bin/cargo` | **no**: EPERM, workspace never loads |
| Any wrapper `rustup` (custom prefix) whose target is `~/.cargo/bin` | yes | `~/.cargo/bin/cargo` | **no** |
| Wrapper whose target is a custom prefix, no `~/.cargo/bin/cargo` | no | `CARGO` | yes |
| Custom `CARGO_HOME` holding the proxies, `rustup` elsewhere first on `PATH` | yes | `$CARGO_HOME/bin/cargo` | **no** (unless under a prefix) |

The failure is quiet: the server starts, `cargo metadata` fails inside
rust-analyzer, and the semantic tier answers nothing for the project
(`FetchWorkspaceError` in the server's log only). The wrapper's own target
is never exec'd by the tree (rust-analyzer does not run `rustup`; GM-603's
`cargo.sysroot` pin stops `rustup component add`), so it needs no rule.

## 3. Options

### A1. Support: allow the proxy rust-analyzer will exec, resolved (recommended)

In `for_server`, add the parent of `canonicalize($CARGO_HOME/bin/cargo[.exe])`
to the toolchain prefixes when that file exists. `cargo_home` is already
an argument (`semantic::cargo_home`, `$CARGO_HOME` else `~/.cargo`, the
same rule rust-analyzer uses). A rustup-init symlink `cargo -> rustup`
canonicalizes to `~/.cargo/bin/rustup`; a hard link stays itself; either
way the directory is the one seatbelt sees on exec. The proxy then execs the
pinned toolchain's binaries (`RUSTUP_TOOLCHAIN` from `pinned_env`), which
are already allowed.

If that canonical file is a script (first bytes `#!`), its own `exec`
target is not resolvable without parsing shell; log one line naming it
("rust-analyzer will run <path>, a script whose target the sandbox does not
allow; the workspace may not load") and allow nothing more.

- Benefits: fixes every row marked "no" above, by the exact path, from
  rust-analyzer's documented lookup, not a prefix guess; works for both
  backends (it is in the shared policy); no shell parsing, no extra process
  at start; the existing rustup-dir rule stays, so nothing allowed today is
  lost.
- Risks: the allowance is a directory, so everything in `$CARGO_HOME/bin`
  (`cargo install`ed tools) becomes exec-able by the tree on installs where
  it was not before. That is already true on every rustup-init install
  (rustup's dir is that directory), the directory stays non-writable
  (`for_server` writes only `registry/`, `git/`), and the tree cannot place
  a file there. Depends on rust-analyzer keeping `prefer_proxy`; if it
  stops, the extra prefix is harmless. A narrower literal-file rule
  (`(literal ...)` for the one proxy) is possible but needs a new rule kind
  in both backends; not worth it while the rustup dir is already a
  directory rule.

### A2. Support by resolving the `PATH` rustup wrapper's target

What the task proposed. Variants:

- Parse the script's `exec` line: fragile (quoting, variables such as
  `$HOME`, env prefixes like Homebrew's `RUSTUP_OVERRIDE_...=`, `exec -a`,
  `/usr/bin/env`), and a misparse allows a wrong directory.
- `rustup which rustc`: names the toolchain binary, already allowed; says
  nothing about where rustup lives.
- Canonical toolchain dir: already allowed (sysroot, cargo/rustc dirs).
- Observe the running binary (`proc_pidpath`, `DYLD_PRINT_*`): racy, and
  SIP strips `DYLD_*` across `/bin/bash`.

None of them addresses what is denied: the tree never execs the wrapper's
target, it execs `$CARGO_HOME/bin/cargo`. A2 would fix the fake-keg case
only because its target equals `~/.cargo/bin`, and would miss the custom
`CARGO_HOME` row. Rejected.

### B. Unsupported: report the semantic tier unavailable with a reason

Before starting the server, if `$CARGO_HOME/bin/cargo` exists and its
canonical directory is not covered by the policy, refuse (`Err` like
`admit`'s `Failed`) with "rust-analyzer would run <path>, which the sandbox
does not allow; put that directory's rustup first on PATH".

- Benefits: no wider exec allowance; the user sees why there are no
  semantic answers instead of a silent empty tier.
- Risks: refuses a setup that A1 makes work at no security cost beyond
  what rustup-init users already have; needs the same path computation as
  A1 anyway; "covered" must replicate each backend's prefix logic
  (seatbelt's `PACKAGE_PREFIXES` is backend-private).

## 4. Recommendation

**A1.** It turns the reported symptom into an exact rule (allow the proxy
rust-analyzer is known to exec) and keeps B's clear-reason behaviour for
the one sub-case that cannot be resolved (a script proxy), as a log line
rather than a refusal. The acceptance's "allows the wrapper's real target
(resolved)" reads, after section 2, as "allows the real exec target,
resolved": that target is `$CARGO_HOME/bin/cargo`, not the wrapper's.

## 5. Test-proxy fallback fix (GM-673/S6 finding)

`link_rustup_proxies` (`plugins/rust/tests/semantic_runs_no_project_code.rs:155-169`)
and `proxy_dir` (`plugins/rust/tests/indexing_writes_nothing_and_stays_offline.rs:170-198`)
look for the sibling `<name>` beside the `rustup` *as found on PATH*. With
`/opt/homebrew/bin` ahead of the keg, that `rustup` is a symlink in a
directory with no `cargo` beside it, so the fallback links to the wrapper
and the proxies run as rustup. Fix: canonicalize the found `rustup` first
(`std::fs::canonicalize`, keep the found path if it fails), then take the
sibling beside the canonical file, else the canonical `rustup`. Homebrew:
`/opt/homebrew/bin/rustup` -> `Cellar/rustup/<v>/bin/rustup`, whose
siblings are the keg's `cargo`/`rustc`/`rust-analyzer` scripts. rustup-init:
unchanged (the file is already canonical). In `real_toolchain`
(`:123-147`) the found path is passed in, so the canonicalization goes in
`link_rustup_proxies` itself, where the unit tests call it directly.

## 6. Edit map

| # | Fn | File | Lines (1-based) | Change |
|---|---|---|---|---|
| 1 | `SandboxPolicy::for_server` | `plugins/rust/src/sandbox.rs` | 93-123 | add `cargo_home.map(|h| h.join("bin").join(format!("cargo{EXE_SUFFIX}")))` -> `canonicalize` -> `parent` to `prefixes`; doc comment names it as "the rustup proxy rust-analyzer runs from `CARGO_HOME`'s `bin/`" |
| 2 | `SandboxPolicy.toolchain` doc | `plugins/rust/src/sandbox.rs` | 41-43 | add "the proxy dir rust-analyzer execs from `CARGO_HOME`" |
| 3 | `server_config_with` | `plugins/rust/src/semantic.rs` | 172-220 | one `log_line!` when that canonical proxy starts with `#!` (helper beside `cargo_home`, `:831-841`) |
| 4 | `link_rustup_proxies` | `plugins/rust/tests/semantic_runs_no_project_code.rs` | 155-169 | canonicalize `rustup` before the sibling rule; doc comment |
| 5 | `proxy_dir` | `plugins/rust/tests/indexing_writes_nothing_and_stays_offline.rs` | 170-198 | same rule as 4 |
| 6 | ADR text | `docs/adr/0039-rust-server-runs-under-an-os-sandbox.md` | 34-35, 122 | list `CARGO_HOME/bin` (proxy) among toolchain prefixes |

New tests: `sandbox.rs` tests module (beside
`for_server_allows_writes_only_to_the_temp_dir_and_cargo_caches`, `:746`);
a seatbelt exec probe using the existing `probe_server`/`exits_zero`/`probe`
helpers (`:599-660`); proxy-fallback tests beside
`proxies_beside_a_wrapper_script_rustup_run_as_their_tool` in both test files.

## 7. Behaviours the tests must pin

- B1. `for_server` with `cargo_home/bin/cargo` a symlink to a `rustup` in another dir puts that dir (canonical) in `toolchain`.
- B2. `for_server` with no `cargo_home/bin/cargo` adds no prefix for it (and still none for `cargo_home` itself or `bin/` writes).
- B3. Seatbelt: a confined process execs `<cargo_home>/bin/cargo` outside the rustup dir and every package prefix (temp dir) and it runs (EPERM without B1).
- B4. The project root still wins: a `CARGO_HOME` inside the root keeps the proxy dir allowed (longer prefix) while the root stays denied (existing ordering, one assertion).
- B5. A script proxy (`#!`) is logged by path, and nothing beyond its own dir is allowed.
- B6. `link_rustup_proxies`/`proxy_dir` given a symlinked `rustup` in a dir without siblings link to the siblings beside the canonical `rustup`.
- B7. End to end (macOS, skip with reason without a rustup-init `~/.cargo/bin/cargo`): with a wrapper keg in temp first on `PATH` exec'ing `~/.cargo/bin`, the sandboxed engine resolves a call (the S4 failure, now green).

## 8. Suggested controls (revert code, never the test)

- C1 (B1, B3, B7): drop the new `prefixes` entry in `for_server`. B1, B3 fail; B7 fails with EPERM in the server log. One build: B1/B3 are lib tests, B7 a `--test semantic_runs_no_project_code` target; group them.
- C2 (B1): use the non-canonical parent (`cargo_home/bin`) instead of the canonical target's parent. B1 fails (symlink target dir missing).
- C3 (B2): add `cargo_home/bin` unconditionally (no existence check). B2 fails.
- C4 (B5): remove the `#!` check. B5 fails (no log line).
- C5 (B6, semantic file): remove the canonicalization in `link_rustup_proxies`. B6 fails.
- C6 (B6, indexing file): same in `proxy_dir`. B6's indexing copy fails.

C5+C6 land in different test binaries: one build.

## 9. Must confirm

1. rust-analyzer's `prefer_proxy` reads `$CARGO_HOME` (not only `~/.cargo`):
   rerun the S4 fake-keg repro with `CARGO_HOME` set to a copy of
   `~/.cargo` and read the path in the server log.
2. Linux (landlock) shows the same exec on a rustup-init host with another
   `rustup` first on `PATH`; A1 covers it through the shared policy, but no
   Linux run has shown the failure.
3. Tools other than `cargo` that rust-analyzer runs through `prefer_proxy`
   in our config (e.g. `rustfmt` is unused by the plugin): all live in the
   same `bin/`, so the directory rule covers them; confirm no other
   absolute proxy path appears in the S4 log after the fix.
4. Owner: accept the reframing (exec target = `$CARGO_HOME/bin/cargo`, not
   the wrapper's target) as meeting the acceptance, and the directory-level
   allowance of `$CARGO_HOME/bin` (section 3, A1 risks).
