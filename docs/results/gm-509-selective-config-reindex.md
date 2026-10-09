# GM-509 selective config reindex: measurements

Slice S25 (measure) of GM-509, the measurement plan of
`docs/architecture/gm-509-selective-config-reindex.md` section 7. The question
is what a watch-file save costs once the TypeScript plugin names what changed,
measured against the whole-language reindex that GM-324 measured
(`docs/results/gm-324-ts-rust-port-measurements.md` section 1). The answer is
used to recalibrate the 30% fallback threshold (owner decision Q3). No
production code or test was changed to produce these numbers.

## Summary

| Measure (excalidraw `1acf66ed`, 658 TS/JS files, 3 reps, median) | base (release-4.3.0) | branch (GM-509) |
|---|---|---|
| E1 `packages/math` `version` bump: files re-extracted | 658 (bulk re-walk) | **0** |
| E1 save -> done (real) | 19.10 s | **0.78 s** |
| E2 `packages/math` `exports` change: files re-extracted | 658 (bulk re-walk) | **94** = the 94 importers |
| E2 save -> done (real) | 17.83 s | 19.61 s |
| E2 CPU, daemon + plugins (user+sys) | 11.4 s | 9.5 s |
| Per-file cost, real / CPU | 28 ms / 17 ms (whole walk / 658) | 200 ms / 101 ms (selective) |
| Break-even share of the language's files | | **14% (real), 17% (CPU)** |

- **Acceptance 1, a bump re-extracts nothing:** yes. 3 of 3 reps logged
  `changed nothing typescript resolution reads - nothing re-extracted`
  (99 / 134 / 151 ms), with no bulk child and no measurable CPU.
- **Acceptance 2, an `exports` change re-extracts only the importers:** yes.
  3 of 3 reps logged `re-extracted 94 file(s)`, which equals the independent
  `git grep` oracle (94 files that import `@excalidraw/math` or a subpath of
  it). There was no fallback and no bulk child.
- **Threshold:** a selective re-extract costs about 6x more per file than the
  bulk walk. At about 14-17% of the language's files it costs as much as
  reindexing the whole language. **Recommendation: lower
  `FALLBACK_SHARE_PERCENT` from 30 to 15**, with the semantic-pass caveat in
  section 4.

## Machine and method

- MacBook, Intel i7-1068NG7 (8 logical CPUs), 32 GB, macOS. The machine was
  shared with other agents' builds and measurements. `uptime` at the start was
  `load averages: 692.03 643.90 570.30` and at the end
  `337.05 367.26 329.15`. The 1-minute load during the reps ranged from 48 to
  548 (per-rep column below). GM-324 ran at a load of about 3.
- Arms: the base is `release-4.3.0` at `404ed48`, built in a throwaway
  `git worktree` and removed afterwards. The branch is
  `feat/GM-509-selective-config-reindex` at `8e184c5`. Both arms are release
  builds (`cargo build --release -p g-mesh -p g-mesh-plugin-typescript`), with
  `target/release` seeded by an APFS clone of the main checkout's.
- Corpus: GM-324's corpus. It is a `git clone --shared` of
  `~/Projects/excalidraw`, detached at
  `1acf66edabc2ac5bbd4aed0714aed7dca7cc2aab`, in the slice scratchpad
  (`gm509-s25/excalidraw`). It has 658 TS/JS files and no `node_modules`.
- Plugin root: `G_MESH_PLUGIN_ROOTS_OVERRIDE` points at a scratch root holding
  only the arm's own `plugins/typescript/plugin.toml`, with two changes:
  - `command` is set to a wrapper that logs each spawn and runs
    `/usr/bin/time -p` around every `--bulk-index` child;
  - `semantic_pass` is set to `false`.

  Both arms are therefore structural only, as in GM-324. Embeddings were off
  in both arms (`G_MESH_MODEL_DIR` set to a missing directory).
- Per session: a fresh `G_MESH_HOME` and a clean corpus. The daemon is
  activated with one MCP `find_definition` call through `g-mesh mcp-shim`,
  which stays connected. The driver waits for `initial index built`, then
  runs E1, waits 5 s idle, runs E2, waits 5 s, and stops the daemon (then
  `kill -9` of its leftover processes).
- Edits, both on `packages/math/package.json`:
  - E1: `"version"` changes from 0.18.0 to 0.18.1. The root `package.json`
    has no `version` field, so a workspace member's file is bumped instead.
  - E2: `exports` gains `"./curve": "./src/curve.ts"`, an existing source file.
- `save -> done` runs from the file write to the arrival of the daemon's log
  line, stamped by a 5 ms tail of `G_MESH_DAEMON_LOG`. On the base, the log
  line is `typescript reindex swapped in`. On the branch, it is the
  `... re-extracted ...` line of `core/src/daemon/config_reindex.rs::run`.
- CPU is the `ps -o time` (user+sys) delta of the session's daemon and its
  long-lived plugin over the edit, plus `/usr/bin/time -p` of the bulk child.
- Arms alternate: rep 1 runs base then branch, rep 2 runs branch then base,
  and rep 3 runs base then branch. The driver (`drive.py`) and the build
  script (`run_all.sh`) are in the slice scratchpad. One background run took
  `real 618.74 user 11.23 sys 16.78` for the driver, after two builds
  (`real 412.54 user 26.15` for the base and `real 427.11 user 15.59` for the
  branch). Real far above user in the builds is time spent runnable at a load
  of 470-690, not compiling: only 4-5 crates were rebuilt.
- Fail-fast checks: the base must show a bulk child on every edit (the
  control). The branch must show no bulk child or fallback line, and E1 must
  log 0 files. No check fired.

## 1. Per-rep results

`dcpu` is the delta of the daemon plus the long-lived plugin. The plugin
binary path contains the daemon's (`.../release/g-mesh`), so `dcpu` already
includes the plugin. `pcpu` is the plugin's share of it. In E1, the
long-lived plugin was spawned by the edit itself (after activation, only the
bulk child had run), so its CPU is not in `dcpu`. The bulk child appears in
`bulk`.

| arm | rep | edit | re-extracted | save->done real | log ms | dcpu | pcpu | bulk real / user / sys | load1 |
|---|---|---|---|---|---|---|---|---|---|
| base | 1 | E1 | all (bulk) | 19.095 | | 6.94 | | 9.44 / 3.78 / 0.28 | 57.7 |
| base | 1 | E2 | all (bulk) | 17.828 | | 7.10 | 0.05 | 10.79 / 4.02 / 0.31 | 47.8 |
| branch | 1 | E1 | 0 | 0.734 | 99 | 0.00 | | none | 68.2 |
| branch | 1 | E2 | 94 | 17.332 | 17022 | 9.79 | 2.13 | none | 63.5 |
| branch | 2 | E1 | 0 | 0.778 | 134 | 0.00 | | none | 257.4 |
| branch | 2 | E2 | 94 | 39.424 | 39110 | 9.39 | 2.07 | none | 270.3 |
| base | 2 | E1 | all (bulk) | 62.075 | | 7.96 | | 36.64 / 4.15 / 0.36 | 495.1 |
| base | 2 | E2 | all (bulk) | 30.251 | | 7.39 | 0.05 | 18.91 / 4.14 / 0.38 | 547.8 |
| base | 3 | E1 | all (bulk) | 18.292 | | 7.39 | | 10.67 / 3.91 / 0.36 | 548.5 |
| base | 3 | E2 | all (bulk) | 17.112 | | 6.52 | 0.05 | 11.02 / 3.76 / 0.32 | 536.4 |
| branch | 3 | E1 | 0 | 0.788 | 151 | 0.01 | | none | 385.5 |
| branch | 3 | E2 | 94 | 19.611 | 19295 | 9.47 | 2.08 | none | 366.7 |

Cold activation (first tool call to `initial index built`) took a median of
30.8 s on the base and 26.1 s on the branch, with bulk child user 4.19 / 4.18 s.

Control: the base showed a bulk child on all 6 edits, and the branch on none.
The two arms differ in the observable the plan names (files re-extracted:
658 against 0 and 94), so the probe measured something.

Waiting: in every row, real time is well above CPU time. On the base, the bulk
child's real time is 2.5-9x its user time. GM-324 attributed the in-daemon
part of this to the child's stdout pipe draining only as fast as core ingests.
On the branch, re-extraction is a serial daemon <-> plugin round trip per file
(`reextract` in `config_reindex.rs` calls `process.reextract` once per file),
so its CPU cannot overlap. That leaves 6-8 s of E2's 17-20 s in which neither
process was on a CPU. At a 1-minute load of 50-550 on 8 CPUs, runnable time in
the run queue is the likely cause, but this run did not separate it from
per-file SQLite commit I/O. Rep 2 of both arms (load 260-550) is the outlier
for both arms alike, and the medians are taken over all 3 reps.

## 2. Against GM-324

| Measure | GM-324 (Rust TS plugin, load ~3) | this run, base (load 48-548) | this run, branch |
|---|---|---|---|
| Watch-file save -> done | 4.26 s | 17.8-19.1 s | E1 0.78 s, E2 19.6 s |
| Re-walk bulk child user / sys | 1.57 / 0.11 s | 3.91-4.02 / 0.32-0.36 s | none |

The base reproduces GM-324's mechanism: a bulk child on every save, and
`swapped in` with `nodes -0 +0, edges -0 +0`. Its absolute times are 4x
GM-324's. Even the bulk child's user time is 2.5x higher, so CPU contention
(shared cores, lower clocks) inflated CPU time as well as waiting. Absolute
numbers are therefore not comparable across the two runs. Ratios within this
run, between arms that alternated under the same load, are comparable.

## 3. Break-even arithmetic

The variables:

- N = 658, the language's files;
- n = the files a delta selects;
- W = the whole-language reindex cost (base, median over E1 and E2);
- F = the selective path's fixed cost (branch E1: the `resolutionChanged` round
  trip, nothing re-extracted);
- c = the marginal cost of one re-extracted file, which is
  (branch E2 - F) / 94.

The selective path wins while F + n·c < W, that is below n* = (W - F) / c.

**Wall clock:**

- W = median(19.095, 17.828, 62.075, 30.251, 18.292, 17.112) = 18.69 s
  (per edit, the medians are 19.10 s and 17.83 s);
- F = 0.778 s;
- c = (19.611 - 0.778) / 94 = 0.2004 s per file;
- n* = (18.69 - 0.78) / 0.2004 = 89.4 files, which is **13.6% of 658**.

By rep:

- rep 1 (load about 60): c = (17.332 - 0.734) / 94 = 0.1766 s and
  W = (19.095 + 17.828) / 2 = 18.46 s, so n* = 100, which is 15.2%;
- rep 3 (load about 370-550): c = 0.2002 s and W = 17.70 s, so n* = 84, which
  is 12.8%.

**CPU (user+sys, daemon + plugins):**

- W = base E2 dcpu plus the bulk child's user+sys: 7.10 + 4.33 = 11.43 s
  (rep 1), 7.39 + 4.52 = 11.91 s (rep 2) and 6.52 + 4.08 = 10.60 s (rep 3),
  with a median of 11.43 s;
- F is about 0 (E1's dcpu is 0.00-0.01);
- c = 9.47 / 94 = 0.1007 s per file (the reps give 9.79, 9.39 and 9.47 s);
- n* = 11.43 / 0.1007 = 113.5 files, which is **17.3%**.

Per file, the whole-language path spends 11.43 / 658 = 17 ms of CPU and the
selective path spends 101 ms, about 6x more. The daemon accounts for about
7.4 s of the selective 9.5 s, or about 79 ms per file. The likely cause is the
per-file `reextract` path (one store transaction and link per file), as
against the bulk walk's batched ingest. This has not been profiled.

## 4. Threshold recommendation

`FALLBACK_SHARE_PERCENT = 30` in `core/src/daemon/config_reindex.rs`. Its only
reader is `daemon::config_reindex::selective`, per g-mesh `find_references`
(see the end of this note).

- **Recommend 15%.** The measured structural break-even is 13.6% of the
  language's files on wall clock and 17.3% on CPU. 15% lies between the two.
  At 30% (197 files on this corpus), the selective path would take about
  0.78 + 197 × 0.20 = 40 s, against about 19 s for the whole-language
  reindex. That is twice the cost of the reindex it is meant to beat.
- **Caveat: the semantic pass.** Both arms ran with `semantic_pass = false`
  (as GM-324). With the pass on, the whole-language reindex is followed by a
  pass over all N files. The selective path runs a scoped pass over its n
  files only (`scoped_semantic_pass`). With s as the semantic cost per file,
  the wall-clock break-even becomes (W/N + s) / (c + s) =
  (28.4 ms + s) / (200 ms + s). This reaches 30% at s ≈ 45 ms per file. The
  TS semantic pass (vtsls) was not measured here. If it costs 45 ms per file
  or more, as GM-425's Rust pass suggests (+60.6 s after swap on this
  machine's g-mesh repo), then 30% remains right while the pass runs, and 15%
  is right when the pass is off, suspended or has no server. Choosing between
  one constant (15%) and a semantic-aware pair is a decision for the owner.
- **The larger lever is the per-file cost, not the threshold.** Selective
  re-extraction costs 6x the bulk walk per file. Batching the selected files
  into one store transaction would raise the break-even share. This would be
  a follow-up task, and nothing in GM-509 changes.

## Not run

- E3 (`tsconfig.base.json` `paths` edit) and E4-E7 (g-mesh repo
  `Cargo.toml` / `go.mod` / `pyproject.toml` / `setup.cfg`) from the plan were
  outside this slice's brief. The Rust, Python and Go deltas belong to the
  split-off tasks (owner Q5).
- The plan's `IMPORTS` edge parity of the E2 branch index against a cold bulk
  index was not checked here. This slice measured the count only.

## Code facts used

- g-mesh `select_project` on `g-mesh-wt-gm509`.
- g-mesh `find_references` on `FALLBACK_SHARE_PERCENT`. It returned one
  resolved reference, in `daemon::config_reindex::selective`, with
  `hasMore: false`.
- The log lines, the per-file `reextract` loop, and
  `plugins/typescript/src/project/facts.rs::delta` (a package's `exports`
  change selects every specifier under `@excalidraw/math`) were read
  directly, each in one known file.
