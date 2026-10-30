# GM-673: plugin-rust tests run `cargo` as rustup on macos-15-arm64

Diagnosis note. CI run 41119255186 (release-4.20.0 @ 8fc5f7e), job
`tests (aarch64-apple-darwin)`, image macos-15-arm64 20261028.0489:
plugin-rust 288 passed, 8 failed. Debug run 41128807428 (throwaway branch
`debug/GM-673-aarch64-cargo`, deleted afterwards) collected the facts below.

## 1. The image's Rust install, and why the tests' `cargo` is rustup

The image ships rustup from Homebrew, keg-only (`brew info`: "rustup: stable
1.29.2 (bottled) [keg-only]"), and no rustup-init install:

| Fact | Evidence (debug run) |
|---|---|
| `PATH` starts with `/opt/homebrew/opt/rustup/bin`; `/Users/runner/.cargo/bin` is on `PATH` but does not exist | `--- PATH`, `ls: /Users/runner/.cargo: No such file or directory` |
| `RUSTUP_HOME`/`CARGO_HOME` unset; toolchains in `~/.rustup` | `--- env` printed only `HOME`; `ls ~/.rustup` |
| `rustup --version` = `rustup 1.29.2 (2026-10-21)`; self-update disabled | step output |
| Every file in the keg's `bin` (`rustup`, `cargo`, `rustc`, `rust-analyzer`, ...) is a ~160-byte **bash script**, not a link | `file`: "Bourne-Again shell script text executable"; `links 1 size 159` |
| `bin/rustup` is `RUSTUP_OVERRIDE_UNIX_FALLBACK_SETTINGS="/opt/homebrew/etc/rustup/settings.toml" exec "/opt/homebrew/Cellar/rustup/1.29.2/libexec/bin/rustup" "$@"` | `strings` on the canonical `rustup` |
| `/opt/homebrew/bin/rustup` is a symlink to the same script | `ls -la` |

rustup picks its mode from the file stem of `argv[0]` (or
`RUSTUP_FORCE_ARG0`). Both test files build a proxy directory by symlinking
`cargo`, `rustc` and `rust-analyzer` to the first `rustup` on `PATH`
(`plugins/rust/tests/semantic_runs_no_project_code.rs` `real_toolchain` ->
`link_rustup_proxies`; `plugins/rust/tests/indexing_writes_nothing_and_stays_offline.rs`
`proxy_dir`). With rustup-init's `~/.cargo/bin/rustup` (a binary) the
symlink's name reaches rustup as `argv[0]` and it runs as cargo. With
Homebrew's wrapper the symlink starts **bash**, and the script's `exec`
replaces `argv[0]` with `.../libexec/bin/rustup`, so every proxy runs as
rustup whatever it is called. Debug run, same shape as the tests:

```
symlink -> PATH rustup: cargo metadata --format-version 1
error: unexpected argument '--format-version' found ... Usage: rustup[EXE] <+toolchain>
symlink / hard link / copy of the canonical rustup: cargo --version -> "rustup 1.29.2"
RUSTUP_FORCE_ARG0=cargo <proxy> --version                    -> "cargo 1.97.1"
/opt/homebrew/opt/rustup/bin/cargo --version                 -> "cargo 1.97.1"
```

Not the cause: argv0-vs-`current_exe` dispatch in rustup itself, a `PATH`
entry ahead of the proxy (the explicit-path call fails too), the tests'
`RUSTUP_*`/`CARGO*` stripping (`env -i` gives the same result).

Reproduced on a rustup-init Mac: a dir of scripts
`exec "$HOME/.cargo/bin/<name>" "$@"`, `cargo` symlinked to its `rustup`
-> the same clap error; `cargo` symlinked to its sibling `cargo` script ->
`cargo 1.97.1`.

The 7 `semantic_runs_no_project_code` failures are the same cause: each
child test fails at a sanity step that runs `real.proxy_dir.join(...)`:

- `:516` (`rust_analyzer_runs_no_build_script_...`): "the unpinned
  `rust-analyzer --version` in the fixture never ran the project's
  `evil-rust-analyzer` hook" - the proxy ran `rustup --version`, which
  succeeds and reads no toolchain file.
- `:964`, `:1253`, `:1337` (`cargo_env_loader_var::*`), `:1783`, `:1841`
  (`sandbox_writes_and_network::*`): `<proxy_dir>/cargo metadata|check` in
  the fixture failed. Their stderr is printed empty because rustup 1.29
  writes its clap usage error to **stdout** (checked locally:
  `<keg>/rustup metadata --format-version 1 >o 2>e` leaves `e` empty, `o`
  holds "error: unexpected argument '--format-version'"), and these
  assertions print only stderr; the indexing test's `plain_cargo` prints
  both. `cargo check` in rustup mode is `rustup check` (update check, exit
  100, stdout only). The tests that use only absolute toolchain binaries
  (`source_freshness::*`, `project_extra_env::*`) passed on the same image.

Why 4.19.0 was green: the previous image installed rustup with rustup-init
into `~/.cargo/bin` (GM-672 had to stop assuming that directory exists).

## 2. Does the plugin misbehave the same way for a Homebrew-rustup user?

No. The plugin never execs a link it made to `rustup`, and never relies on
`argv[0]` dispatch:

- `plugins/rust/src/semantic.rs:389` `Toolchain::detect`: runs `rustup` by
  name only for `rustup default` / `rustup which --toolchain <pin> <name>`
  (`rustup()` at `:952`) - rustup mode is what it wants, and the wrapper
  gives it. `rustc`, `cargo`, `rust-analyzer` are the absolute toolchain
  binaries `rustup which` prints (`~/.rustup/toolchains/<pin>/bin/...`, as
  the CI log shows for rust-analyzer).
- `:632` `pinned_env`: `RUSTC`, `CARGO_BUILD_RUSTC`, `CARGO` absolute, plus
  `RUSTUP_TOOLCHAIN`, so rust-analyzer and cargo reach no proxy.
- `:895` `candidates` / `:924` `is_rustup_proxy`: the keg's
  `rust-analyzer` script canonicalizes into the same directory as the
  canonical `rustup` (`/opt/homebrew/Cellar/rustup/1.29.2/bin`), so it is
  still recognised as a proxy and skipped.
- `plugins/rust/src/sandbox.rs:196` `PACKAGE_PREFIXES` includes
  `/opt/homebrew`, so the wrapper's `libexec/bin/rustup` stays executable
  under seatbelt should anything reach it.

The passing semantic tests on this image (all but the 7, which fail only in
their sanity steps) are the evidence that the product path works with this
install.

## 3. Fix shapes

| Shape | Benefit | Risk |
|---|---|---|
| **A. Test-only: link each proxy to rustup's own proxy beside the `rustup` found, falling back to `rustup`** | Fixes the tests for every install shape (rustup-init, Homebrew keg wrappers, old Homebrew with only `rustup`); a developer with Homebrew rustup gets passing local runs too; no CI cost | A file named `cargo` beside `rustup` that is not rustup's proxy would be linked; the tests' own sanity steps (hooks must fire) fail loudly in that case rather than pass silently |
| B. CI-only: install rustup with rustup-init into `~/.cargo/bin` and put it first on `PATH` | Restores the 4.19.0 shape exactly; no code change | Leaves the tests broken for any Homebrew-rustup developer; hides a supported install shape from CI; one more download per job; the next image change can break it again |
| C. Plugin change | - | Nothing to fix: the plugin is not affected (section 2) |

**Recommended: A.**

### Edit map (A)

1. `plugins/rust/tests/semantic_runs_no_project_code.rs`
   - `real_toolchain`: keep finding `rustup_bin` (first `PATH` dir holding
     `rustup`); pass it unchanged.
   - `link_rustup_proxies(dir, rustup)`: for each `name`, the link target
     is `rustup.with_file_name(format!("{name}{EXE_SUFFIX}"))` when that is a
     file, else `rustup`. Rewrite the doc comment: rustup dispatches on
     `argv[0]`, which a wrapper script's `exec` replaces, so the install's
     own proxy is linked when it has one (rustup-init's `~/.cargo/bin` and
     Homebrew's keg `bin` do), and `rustup` itself otherwise.
2. `plugins/rust/tests/indexing_writes_nothing_and_stays_offline.rs`
   `proxy_dir`: the same target rule and the same comment change.

No production code, so per the testing rules only the plugin-rust section
needs to run.

### Control

Reproduce the Homebrew shape on any rustup-init Mac with a fake keg ahead
on `PATH` (no Homebrew needed):

```
keg=$(mktemp -d)
for n in rustup cargo rustc rust-analyzer; do
  printf '#!/bin/bash\nexec "%s/.cargo/bin/%s" "$@"\n' "$HOME" "$n" > "$keg/$n"; chmod +x "$keg/$n"
done
PATH="$keg:$PATH" cargo nextest run -p g-mesh-plugin-rust \
  --test indexing_writes_nothing_and_stays_offline --test semantic_runs_no_project_code
```

(The outer `cargo` then runs through the keg's `cargo` script, which works;
the tests find the keg's `rustup` first, which is the CI shape. The parent
tests spawn their ignored children themselves.) With the fix: green. Control (revert the target rule in both helpers, keep
the fake keg): the indexing test fails at `:323` with "Usage: rustup" and
the 7 semantic tests fail "did not pass in its environment" - the CI
failure, locally. Without the fake keg, both arms pass (the control is the
keg, not the machine). CI aarch64 on macos-15-arm64 is the standing check.
