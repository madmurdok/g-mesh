# 0037. The rust tier refuses a cargo config `[env]` that sets a dynamic-loader variable

## Status
Accepted 2026-10-14 (GM-587, owner decisions D1-D3). Experiments, the
mechanism comparison and the remaining channels:
[`GM-587-cargo-env-loader-vars.md`](../design/GM-587-cargo-env-loader-vars.md).
Follows [ADR 0032](0032-semantic-tiers-execute-no-project-code.md) and
[ADR 0033](0033-semantic-tiers-reach-no-network-and-write-nothing.md).
Superseded in part on macOS by
[ADR 0039](0039-rust-server-runs-under-an-os-sandbox.md): the server runs
under Seatbelt there, and this pre-check stays as a second layer.

## Context
ADR 0032 keeps a project's `.cargo/config.toml` from naming the `rustc`,
the wrapper or the `rustflags` that cargo runs for rust-analyzer, by
environment variables that outrank the config. The config's `[env]` table
has no such variable. Cargo applies it to the `rustc` children of
`cargo metadata` after they inherit the server's environment, and an entry
with `force = true` overrides any inherited value. An entry for a
dynamic-loader variable (`DYLD_INSERT_LIBRARIES` on macOS, `LD_PRELOAD` on
Linux) therefore loads a project-supplied library into those children on
the first index of a cloned project: measured on macOS, about 27 `rustc`
and 2 `rustup` children loaded it with the full ADR 0032 environment in
place. Emptying the variable in the server's environment, or through
rust-analyzer's `cargo.extraEnv`, did not stop it.

## Decision
- **D1 - refuse the semantic tier when a cargo config in effect sets a
  loader variable in `[env]`.** Before the server starts, the plugin reads
  the configs cargo reads for the project root (`.cargo/config.toml` and
  `.cargo/config` in the root and every ancestor, then `$CARGO_HOME`'s,
  following `include` transitively, plus the root's immediate
  subdirectories, where rust-analyzer runs `cargo metadata` when the root
  has no `Cargo.toml`). If any `[env]` names one of the keys below, in the
  scalar or the `{ value, force, relative }` form, the engine factory
  returns `Err` naming the key and the file. The SDK's `LazyEngine` logs it
  once and keeps the plugin structural-only for the process
  (`plugins/rust/src/semantic.rs`, `engine` and `cargo_env_loader_var`).
  A config that does not parse is matched by text, so a syntax the plugin's
  TOML parser rejects and cargo accepts cannot hide a key.
  The check fails closed: a config path that exists but cannot be `stat`ed
  or read (any error but "not found"), or is not a regular file, refuses
  the tier the same way, naming the file and the error. The check runs
  again before every pass, server start and `workspaceChanged` (which
  then asks rust-analyzer to reload); a config that turns hostile later
  stops the server and leaves the plugin structural-only for the rest of
  the process. rust-analyzer runs no file watcher of its own: the bridge
  claims client file watching (`didChangeWatchedFiles` dynamic
  registration, with rust-analyzer's default `files.watcher = "client"`)
  and forwards, as `workspace/didChangeWatchedFiles`, only the `.rs` files
  core sends a pass, inside the checked pass. rust-analyzer refetches the
  workspace (`cargo metadata`) on a created or deleted file of any kind and
  on a `Cargo.toml`, `Cargo.lock` or `.cargo/config*` change, but not on an
  edit to an existing `.rs` file (`should_refresh_for_change` and
  `process_changes`, rust-analyzer 1.97.1), so every forwarded event is
  sent after a clean re-check. A manifest or config edit is never
  forwarded: it reaches `cargo metadata` only through the reload
  `workspaceChanged` asks for after a clean re-check. A config added
  together with a manifest edit is therefore refused before rust-analyzer
  reloads. Without the forwarding, an edit to a file no pass asks about
  never reached the server, and answers in files that depend on it went
  stale.
- **D3 - the key list is broad:** `DYLD_INSERT_LIBRARIES`,
  `DYLD_FRAMEWORK_PATH`, `DYLD_LIBRARY_PATH`, `LD_PRELOAD`, `LD_AUDIT`,
  `LD_LIBRARY_PATH`.
- **D2 - direction: an OS sandbox of the rust-analyzer process tree is the
  structural fix**, split per OS into its own tasks (macOS Seatbelt with a
  default-deny `file-map-executable` allowlist; Linux Landlock with a
  `bwrap` fallback). It is not part of this decision's code. When it lands,
  the refusal stays as defense in depth.

Rejected: neutralizing the variable by environment or `cargo.extraEnv`
(loses to `force = true`); a clean `CARGO_HOME` alone (leaves the project's
own config); a prebuilt `rust-project.json` (computing it runs
`cargo metadata`, which is the injection).

## Consequences
- A project whose cargo config sets any of the six keys in `[env]` has no
  semantic tier: its Rust answers carry the provenance of a missing tier,
  and the log names the key and the file. A legitimate FFI crate that sets
  `LD_LIBRARY_PATH` there is downgraded too (the cost of the broad list).
- The check reproduces cargo's config discovery rather than asking cargo,
  since asking cargo is what runs the children. A discovery rule cargo adds
  later is not covered until the check learns it; reading more files than
  cargo does only over-refuses.
- The refusal closes only the `[env]` loader channel. A config `rustc`
  wrapper, a project `rust-analyzer.toml`, a `[target.*]` runner or linker,
  and build scripts or proc macros once enabled are left to ADR 0032's
  precedence or to being disabled, until the sandbox (D2) contains them as
  a class.
