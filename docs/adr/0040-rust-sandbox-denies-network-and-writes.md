# 0040. The rust server's sandbox denies network and writes outside its temp dir and the cargo caches

## Status
Accepted 2026-10-21 (GM-640, owner decisions D1 A, D2 A, D3 A).
Experiments, the profile, the Linux rules and the behaviours tests pin:
[`GM-640-sandbox-network-write-deny.md`](../design/GM-640-sandbox-network-write-deny.md).
Supersedes [ADR 0039](0039-rust-server-runs-under-an-os-sandbox.md) D4 A
("the profile governs mapping and exec only") and its consequence "No Linux
layer confines writes". Makes the OS enforce what
[ADR 0033](0033-semantic-tiers-reach-no-network-and-write-nothing.md)
enforced only by configuration.

## Context
ADR 0033 keeps rust-analyzer and the cargo it runs offline and writing
nothing through configuration: `CARGO_NET_OFFLINE`, initialization options,
rust-analyzer's own `-Zlockfile-path`. ADR 0039 put the server's process
tree under an OS sandbox that decides only mapping and exec. A tool that
ignores the configuration, or a future rust-analyzer that changes it, could
still reach the network or write into the project.

## Decision
- **Writes.** The tree may write only the server's private temp dir, set as
  its `TMPDIR`, and `$CARGO_HOME/registry` and `$CARGO_HOME/git` (each when
  it exists), plus the OS's `/dev`. The project root is read-only even inside
  a writable prefix; a writable prefix inside the root stays writable.
- **D1 A - a private `TMPDIR` per engine.** `server_config_with` creates
  `<temp>/g-mesh-rust-<pid>-<n>` (mode 0700) on macOS and Linux; the user's
  shared temp dir is not writable.
- **D2 A - `CARGO_HOME` writable only in `registry/` and `git/`.** Never
  `$CARGO_HOME` itself, `bin/`, `config.toml`, `credentials.toml` or `env`,
  which reach the user's own later cargo runs.
- **Network.** Nothing in the tree may connect, bind or listen; the server
  talks over stdio. macOS: `(deny network*)`. Linux:
- **D3 A - a network namespace plus Landlock TCP.** The mount layer's
  `unshare` adds `CLONE_NEWNET` (only a down loopback), and Landlock keeps
  denying TCP bind and connect from ABI 4 and pathname unix-socket connects
  from ABI 9.
- **Linux writes.** The mount layer makes every mount read-only except the
  writable prefixes and the root read-only again; Landlock handles every
  write right of the kernel's ABI and grants it only beneath the writable
  prefixes and `/dev`.

## Consequences
- An offline project whose dependencies are not cached still resolves
  nothing (unchanged), and now no tool in the tree can fetch them either.
- A project without a `Cargo.lock` gets no lockfile written into it by any
  rust-analyzer version on macOS and Linux.
- The Landlock-only tier (no user namespace) does not block UDP, blocks TCP
  only from ABI 4, pathname unix sockets only from ABI 9 and truncation only
  from ABI 3, and cannot keep a root that lies under a writable prefix
  read-only; its `Partial` gap names each hole (`landlock_gaps`).
- One empty `g-mesh-rust-*` directory per engine is left for the OS's temp
  cleaning.
