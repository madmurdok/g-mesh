# 0027. Plugin fetch verifies checksums in Rust, a third implementation of the install rule

## Status
Accepted 2026-10-06 (owner review of the design note
[`GM-353-per-plugin-release-assets.md`](../design/GM-353-per-plugin-release-assets.md),
D2). The Rust fetch itself is GM-331; this records the decision it follows.

## Context
GM-353 publishes one asset per bundled plugin and language
(`g-mesh-plugin-<lang>-v<ver>-<target>.tar.gz` plus a sibling `.sha256`, all
listed in `SHA256SUMS`). Fetching one happens inside `g-mesh plugins install`,
a Rust binary on every platform, Windows included. Neither `install.sh` (POSIX
sh) nor `install.ps1` (PowerShell) can be called from it or shared with it, and
the two scripts must stay standalone because they run before any g-mesh binary
exists. A checksum rule therefore already lives in two places, and the fetch
needs it a third time.

## Decision
The plugin fetch verifies checksums in Rust, reusing core's existing download
in `cli/model.rs` (`download_to_partial`: streams to `.partial`, hashes while
writing, renames only on success). GM-331 extracts it into a shared helper. One
difference from the model download: the expected digest comes from the sibling
`.sha256` fetched from the same release, as `install.sh` does, not from a
constant.

The discipline is the one in `install.sh`'s CHECKSUMS header: per-asset
`.sha256`, compare before unpacking, refuse and delete on mismatch, print both
digests.

Each copy carries only the invariant in a comment ("the same rule is
implemented in the other two; change all three") and links to this ADR:
`install.sh` and `install.ps1` (CHECKSUMS sections, GM-353's code slice) and
the Rust helper's module doc (GM-331).

## Consequences
- One Rust download path for model and plugin, already tested; no shell
  dependency on Windows.
- Three implementations of the same rule (sh, ps1, Rust) can drift. The ADR
  link from each copy is the mitigation.
- GM-353 proves the rule by a manual install (curl, `shasum -c`, `tar -xzf`);
  the Rust record lands only with GM-331.

## Rejected alternatives
- **`install.sh --plugin <lang>` / `install.ps1 -Plugin`.** Shares code with
  `install.sh` literally, but adds two more front doors beside the one GM-331
  specifies, and Windows still needs the ps1 copy.
