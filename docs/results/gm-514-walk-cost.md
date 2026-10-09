# GM-514: walk cost before/after the core symlink table (M4)

Must-confirm item M4 of
[gm-514-core-symlink-table.md](../architecture/gm-514-core-symlink-table.md) §6:
with link following, `IgnoreLayers::load` runs on every settled directory
(GM-508), so its cost and that of `project_walk::project_files` are measured
before and after the change.

- **Before:** release-4.3.0 at `a02e43f` (the branch's base).
- **After:** `feat/GM-514-core-symlink-table` at `b70572b`.

## Results

Median / p90 in ms over 30 iterations per run, two interleaved runs per arm
(order: before, after, before, after). `user`/`sys`/`real` are
`/usr/bin/time -p` for the whole run (warm-up + 30 loads + 30 walks).

| Corpus (files walked) | Arm | `load` med / p90 | `project_files` med / p90 | user s | sys s | real s |
|---|---|---|---|---|---|---|
| g-mesh (749) | before | 24.1 / 27.2 · 23.6 / 39.7 | 25.8 / 36.6 · 23.8 / 50.4 | 0.34 · 0.31 | 1.09 · 1.00 | 2.30 · 2.03 |
| g-mesh (749) | after | 30.8 / 36.7 · 32.3 / 49.1 | 28.3 / 36.2 · 34.8 / 48.2 | 0.60 · 0.64 | 1.09 · 1.17 | 2.27 · 2.43 |
| excalidraw (1262) | before | 39.0 / 63.1 · 36.6 / 56.0 | 39.9 / 74.4 · 33.2 / 45.3 | 0.69 · 0.64 | 1.25 · 1.14 | 3.13 · 2.58 |
| excalidraw (1262) | after | 35.5 / 110.7 · 33.7 / 41.3 | 39.2 / 70.8 · 34.7 / 40.8 | 0.85 · 0.84 | 1.22 · 1.15 | 3.46 · 2.35 |
| fixture (before 201 / after 251) | before | 3.0 / 4.1 · 3.0 / 3.9 | 3.3 / 7.9 · 3.2 / 4.7 | 0.07 · 0.06 | 0.14 · 0.14 | 0.66 · 0.39 |
| fixture (before 201 / after 251) | after | 5.8 / 6.6 · 7.2 / 9.6 | 6.0 / 7.0 · 7.6 / 10.6 | 0.17 · 0.19 | 0.20 · 0.24 | 0.82 · 0.87 |

**Control (tells the arms apart):** on the fixture the before arm walks 201
files and the after arm 251: the 50 files of the gitignored `vendor/lib`,
reached through `src/vendored -> ../vendor/lib`, appear only when links are
followed. The in-root alias `src/alias -> d0` adds nothing in the after arm
(the guard lists each real file once). The real corpora walk the same file
count in both arms.

**Links in the corpora** (`find -type l`, pruning `target`, `node_modules`,
`.git`): g-mesh has **3**, all git-tracked, in-root aliases under
`eval/embedding/confirm/` (`variants.toml`, `corpora.toml`,
`queries/mechanical`); excalidraw (`1acf66ed`, clean) has **0**; the fixture
has 3. So g-mesh exercises the after arm's link path, excalidraw does not.

## Gitignore gate's full-walk fallback on g-mesh

`project_files_under` falls back to a full `project_files` walk when its
pruned walk meets a symlink inside a subtree or among a subtree's ancestors'
entries. All 3 links sit under `eval/embedding/confirm/`; of the 6 tracked
`.gitignore` files only the root one's subtree (`""`) reaches them. So the
fallback fires on a change to the root `.gitignore` and on none of the 5
under `plugins/`. Its cost is one full walk: ~25-35 ms here. The main
checkout's 3 further links are under `/eval/embedding/work/`, which the root
`.gitignore` ignores, so they are out of reach.

## Machine state

Intel i7-1068NG7, 8 logical CPUs. Load averages during the timing runs:
**717-731** (1 min), ~654 (5 min), from another agent's test suite. Every
run is mostly waiting: `user + sys` is 0.2-2.1 s against 0.4-3.5 s `real`,
and wall-clock medians carry that wait. Compare `user` (CPU the process
itself spent) for the cost of the code; read the ms columns as indicative
only.

## Verdict

There is a regression, and it is small in absolute terms.

- **g-mesh (3 links):** `IgnoreLayers::load` median goes from 24 to 31-32 ms
  (about +30%, +7-8 ms per settled directory). `project_files` goes from
  24-26 to 28-35 ms. Process `user` CPU nearly doubles (0.31-0.34 s to
  0.60-0.64 s); `sys` is unchanged.
- **excalidraw (no links):** wall-clock medians show no difference within the
  noise (load 37-39 vs 34-35 ms, walk 33-40 vs 35-39 ms). `user` CPU rises
  ~25% (0.64-0.69 s to 0.84-0.85 s): the link-following walker costs a little
  CPU even when there is no link.
- **fixture:** about 2x (3 to 6-7 ms), partly because the after arm walks 25%
  more files.

At ~30 ms per load on a 750-file project the change does not threaten the
watcher's settle path. Under a load average of ~720 the wall-clock deltas
are not precise; a quiet-machine rerun would tighten the ~+30% g-mesh figure,
but the direction (more user CPU in the after arm) holds in both rounds on
every corpus.

## Method

- A throwaway release-mode example (`core/examples/gm514_walk_cost.rs`, never
  committed), identical in both arms, built with `cargo build --release -p
  g-mesh --example` into one shared scratch target dir. It canonicalizes the
  root, does one warm-up `project_files(root, &[]).count()` (its count is
  the reported file count) and one `IgnoreLayers::load(root)`, then 30
  iterations each timing one `IgnoreLayers::load` and one
  `project_files(..).count()`, and prints median and p90.
- Before arm built in a throwaway `git worktree --detach` at `a02e43f`,
  removed afterwards; the corpora (the g-mesh task worktree, excalidraw,
  the fixture) are the same paths for both arms.
- Arms interleaved before, after, before, after; one script, `uptime` before
  each round, `/usr/bin/time -p` per run.
- Fixture: 10 dirs x 20 files under `src/`, a `.gitignore` with `vendor/`,
  50 files in `vendor/lib/`, and three links: `src/vendored -> ../vendor/lib`,
  `src/alias -> d0`, `src/f0link.rs -> d0/f0.rs`.
