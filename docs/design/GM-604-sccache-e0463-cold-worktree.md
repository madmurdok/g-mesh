# GM-604: sccache and E0463 on cold worktree builds

## Question

GM-584/S1 reported that a cold `cargo build` in a fresh worktree, run through
the user-level sccache wrapper (`~/.cargo/config.toml`:
`[build] rustc-wrapper = "~/.cargo/bin/sccache"`), failed with E0463 ("can't
find crate for `toml_edit`/`serde`") on rmeta files that existed, and that the
same build with `RUSTC_WRAPPER=` succeeded in 129 s. Is sccache serving
unreadable artifacts?

## Setup (read only)

- sccache 0.18.0, local disk cache `~/Library/Caches/Mozilla.sccache`,
  10 GiB of 10 GiB used (LRU eviction active), no config file, no `SCCACHE_*`
  env, `Base directories (none)`.
- rustc/cargo 1.97.1, x86_64-apple-darwin.
- Lifetime stats: 0 cache read errors, 0 write errors, 0 cache errors.

How sccache 0.18 keys a rustc call (`src/compiler/rust.rs`, hash steps 1-10):
the command line minus `--extern`, `-L`, `--out-dir`, `--check-cfg`; the
digests of source files and of every `--extern` file (so a dependency's rmeta
content is part of the key); env vars listed in dep-info; **the cwd**; the
compiler version. Workspace crates carry `-C incremental` and are never cached;
`bin`/`proc-macro`/`dylib` crates are not cacheable either. Registry crates are
compiled with cwd = their `~/.cargo/registry/src/...` directory, which is the
same in every worktree, so they hit across worktrees without `basedirs`.
A compile runs **in the sccache server process**, not in the client cargo
spawned.

## Reproduction

One background script (`scratchpad/gm604/run.sh`), three arms, all with the
wrapper:

| Arm | What | Load (1-min, start/end) | real | user | sys | Result | Rust hits / misses* |
|---|---|---|---|---|---|---|---|
| A | cold `cargo build --workspace` in `g-mesh-wt-gm604` (no `target/`) | 549 / 178 | 334 s | 226 | 50 | **ok** | 216 / 51 |
| B | cold build in a second fresh worktree, warm cache | 18 / 550 | 597 s | 191 | 43 | **ok** | 170 / 43 |
| C | B's `target/` replaced by an APFS clone of A's, then build | 553 / 491 | 333 s | 133 | 21 | **ok** | 0 / 0 (15 non-cacheable) |

\* Deltas of the server's global counters; other agents built concurrently,
so they are an upper bound for this build. Read errors stayed 0 in all arms.

`real` far above `user+sys` is waiting on a machine at load 180-610 (other
agents' suites), not sccache. **E0463 did not reproduce**, including at loads
higher than GM-584's (250-354), and cross-worktree hits work (arm B: 170 Rust
hits, no read errors).

## What GM-584 actually observed (from its transcript)

1. 15:04 and 15:06: two launches of the measurement script collided in the
   same worktree. Its `target/` was a `cp -c -R` clone of the main checkout's
   32 GB `target/`, taken while that checkout could be building. The stop
   command (`kill -- -$PG`) ran with an **empty pgid**, so it killed nothing;
   at that moment a `sccache rustc --crate-name toml_edit` of the stale run was
   still compiling into the same `target/debug/deps`.
2. 15:08: E0463 for `serde`/`chrono` in `schemars`, files present (827 KB /
   2 MB, written 15:07).
3. 15:08: `cargo clean -p serde -p chrono -p schemars` and then
   **`RUSTC_WRAPPER= cargo build`: the same E0463** (`serde`, `chrono`,
   `serde_json`). So E0463 occurred **without sccache** too.
4. 19:17 (load 250-354): `cargo clean` + cold build with the wrapper: E0463 on
   `toml_edit` after 22 s.
5. 19:19 (load 21): `cargo clean` + cold build with `RUSTC_WRAPPER=`: ok, 129 s.

Step 5 changed two things at once against step 4 (the wrapper and a calm
machine with nothing else in that `target/`), and step 3 already contradicts
"works without the wrapper".

## Cause

**Ruled out as an sccache defect we can reproduce.** Evidence: three
wrapper builds (cold, cold cross-worktree, cloned target) succeed under loads
up to 600 with zero cache read errors; in GM-584 the error also appeared
without the wrapper.

Most likely cause in GM-584: **concurrent writers into one `target/`**: the
two colliding script launches and the stale run nobody stopped (step 1), on a
target cloned from a checkout that may have been mid-build. Cargo's build lock
protects one cargo against another, but it does not cover a compile that
outlives its cargo, and with sccache that is the normal case after a kill:
the rustc runs as a child of the sccache server, so killing cargo (or its
process group) leaves it running, and it writes its rmeta/rlib into
`--out-dir` after the next build has already taken the lock and read the old
or half-written file. A rustc that cannot load the `--extern` file reports
E0463 ("can't find crate"), even though `ls` shows the file afterwards.

Not proven: step 4 (19:17) is four hours after the collision. I found no record
of what else wrote into `g-mesh-wt-gm584/target` then, and the original build
log was overwritten. It stays unexplained; a recurrence should be captured with
the triage steps below before anything is changed.

## Options

| | Change | Benefit | Risk |
|---|---|---|---|
| 1 (Recommended) | Keep the config; document triage and the kill rule (edits below) | Keeps the shared cache (170-216 Rust hits per cold worktree build) | A recurrence still costs one rebuild; relies on agents following the doc |
| 2 | Repo-level opt-out: in `g-mesh/.cargo/config.toml` add `[build]` `rustc-wrapper = ""` | g-mesh builds can no longer involve sccache | Gives up the cross-worktree cache that the worktree workflow relies on; every cold worktree build compiles about 260 more crates; addresses a cause we did not show |
| 3 | Remove `rustc-wrapper` from `~/.cargo/config.toml` | Same as 2 for every project | Same cost for all Rust projects |
| 4 | `SCCACHE_BASEDIRS` / `--remap-path-prefix` | More hits for crates whose key contains a worktree path | Unrelated to E0463; separate tuning |
| 5 | Raise the cache size (`SCCACHE_CACHE_SIZE`, now 10 GiB and full) | Fewer evictions, more hits | Disk; unrelated to E0463 |

## Recommendation and exact edits

Option 1. No change to `~/.cargo/config.toml` or the sccache config.

The worktree setup rules live in the owner's `~/.claude/CLAUDE.md` and memory,
not in this repo, so the owner applies these:

1. `~/.claude/CLAUDE.md`, section *Git*, after the throwaway-worktree bullet,
   add:

   > - **A killed build keeps writing.** With the sccache wrapper, rustc runs
   >   under the sccache server, so killing cargo or its process group does not
   >   stop compiles in flight; they write into `target/` later. Before
   >   rebuilding in that `target/`, check that
   >   `pgrep -fl -- "--out-dir <wt>/target"` is empty. Never start two builds
   >   in one worktree's `target/` from different scripts. Clone a `target/`
   >   only from a checkout with no build running.

2. Same file, add an E0463 triage line (or a memory note):

   > - **E0463 on an rmeta that exists:** first `pgrep -fl -- "--out-dir
   >   <wt>/target"` (a stale writer), then `cargo clean -p <crate>` and
   >   rebuild **with** the wrapper. Only if it repeats on a quiet target,
   >   rebuild once with `RUSTC_WRAPPER=` **without** cleaning anything else,
   >   and keep both logs plus `sccache --show-stats` for GM-604. Do not change
   >   two things at once.

Side finding: `~/.claude/hooks/wt-cleanup.sh` refused to remove a detached
worktree at `aad0515` ("not merged into any local branch"), although
`release-4.10.0` and the task branch both point at that commit; `--force` was
needed.
