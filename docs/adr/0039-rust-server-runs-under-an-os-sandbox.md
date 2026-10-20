# 0039. The rust server runs under an OS sandbox that denies mapping and exec of project code

## Status
Accepted 2026-10-20 (GM-600, owner decisions D1 A, D2 A, D3 B, D4 A).
Experiments, the profile and the behaviours tests pin:
[`GM-600-macos-seatbelt-sandbox.md`](../design/GM-600-macos-seatbelt-sandbox.md).
Supersedes in part, on macOS, [ADR 0037](0037-rust-tier-refuses-cargo-env-loader-vars.md);
follows [ADR 0032](0032-semantic-tiers-execute-no-project-code.md).
The Linux backend is GM-601.

## Context
ADR 0037 closes one channel by which a project gets its code mapped into a
process the rust tier starts: a cargo config `[env]` dynamic-loader
variable. It is a pre-check of known files and variables. The class it
belongs to is wider: any library the loader maps, or any binary cargo or
rust-analyzer execs, from under the project root. An OS sandbox around the
whole rust-analyzer process tree closes the class rather than the channel.
On macOS, `sandbox-exec` (Seatbelt) is present and working on 26.6.2,
deprecated but unchanged since 2017; it execs its target in place and its
rules apply to every descendant.

## Decision
- **The server's command is wrapped in this OS's sandbox**
  (`plugins/rust/src/sandbox.rs`, called from `semantic::server_config`).
  On macOS: `/usr/bin/sandbox-exec -p <profile> -D ROOT=… -D ALLOW_n=… --
  <rust-analyzer> <args…>`. The profile is `allow default` with
  `file-map-executable` and `process-exec` denied, then allowed under the
  OS (`/System`, `/usr/lib`, `/Library/Apple` to map; `/bin`, `/usr/bin`,
  `/usr/libexec`, `/System` to exec) and the toolchain (sysroot, rustup
  proxies dir, the server's, `rustc`'s and `cargo`'s dirs), and denied
  under the canonical project root. Paths reach the profile only as `-D`
  parameters, all canonical (Seatbelt matches resolved paths). Seatbelt
  applies the last matching rule, so rules are emitted in ascending path
  length: the most specific wins, the root deny covers a project inside an
  allowed prefix, and a toolchain prefix inside the root stays allowed.
  A `--version` preflight of the wrapped command runs once per engine.
- **D1 A - a sandbox that cannot be applied refuses the semantic tier.**
  `sandbox-exec` missing, a profile it rejects (exit 65), an exec it denies
  (exit 71) or any other failed preflight makes `engine` return `Err` with
  the reason; the plugin stays structural and logs it. No unconfined
  rust-analyzer starts on macOS. An OS with no backend yet logs that and
  starts the server unconfined, with ADR 0037's pre-check in force.
- **D2 A - a hostile project still reaches the semantic tier for its own
  workspace.** Under the sandbox, `cargo metadata` with dependencies fails
  on such a project (dyld aborts a `rustc` whose inserted library is
  blocked); rust-analyzer falls back and resolves workspace-local calls.
  The test pins no marker written and a workspace-local call resolved.
- **D3 B - package-manager prefixes are allowed too:** `/opt/homebrew`,
  `/usr/local`, `/opt/local` and `/nix/store`, each when it exists, so a
  Homebrew or nix toolchain and its libraries load. They take part in the
  length ordering, so a project inside one is still denied.
- **D4 A - the profile governs mapping and exec only.** File reads, writes
  and network stay as ADR 0033 leaves them; denying them is a separate task.

## Consequences
- On macOS a project can make the rust tier map or exec none of its own
  code, whichever channel it uses; ADR 0037's pre-check stays as a second
  layer and is the only layer on an OS without a backend.
- An allowlist miss on an unusual install costs that user the semantic
  tier until fixed; the logged reason carries `sandbox-exec`'s message.
- Bridge errors that print the server command name `/usr/bin/sandbox-exec`;
  the start-up log names the confined server and the allowed prefixes.
- A project proc-macro dylib (under the root) cannot be loaded, matching
  `procMacro.enable = false`; the sysroot's proc-macro server is allowed.
- If Apple removes `sandbox-exec`, the preflight reports it missing and the
  tier is refused, not silently unconfined.
